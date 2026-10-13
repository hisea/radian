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
    Agent, AgentConfig, AgentEvent, ContextBudget, EventSink, InputSource, NoTools, ToolCall,
    ToolContext, ToolError, ToolFailure, ToolOutput, ToolProvider, ToolSession, ToolSpec,
    TurnOptions,
};
use agent_provider_openai_compatible::{ApiKey, OpenAiCompatible, OpenAiCompatibleConfig};
use clap::Parser;
use radian_coding::{CodingSession, CodingTools};
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
    about = "Full-screen TUI for the radian agent runtime"
)]
struct Cli {
    #[arg(long)]
    model: String,

    /// Everything before `/chat/completions`.
    #[arg(long, default_value = "https://api.openai.com/v1")]
    base_url: String,

    /// Environment variable holding the API key. Never read from anywhere else, never printed.
    #[arg(long, default_value = "OPENAI_API_KEY")]
    api_key_env: String,

    /// Extra request header, repeatable: `--header NAME=VALUE`.
    #[arg(long = "header", value_name = "NAME=VALUE")]
    header: Vec<String>,

    /// Where session files live. Defaults to `RADIAN_SESSION_DIR`, then the platform's data
    /// directory (`~/.local/share/radian/sessions`, or `Library/Application Support` on
    /// macOS). The same convention as `agent-cli`.
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

    /// Expose the built-in file and read-only git tools rooted at this directory
    /// (read/write/patch/list, git_status/git_diff). No shell is provided.
    #[arg(long, value_name = "DIR")]
    workspace: Option<PathBuf>,

    /// The permission tier (ADR 0012): `safe` denies file modification, `workspace-write`
    /// allows writes that resolve inside the workspace, `full-access` also allows MCP.
    #[arg(long, value_name = "TIER", default_value = "workspace-write")]
    profile: String,

    /// How `ask` is resolved (ADR 0012's ApprovalPolicy): `on-ask` asks when a rule says so,
    /// `always` asks for every call a rule does not deny, `never` turns `ask` into a denial.
    #[arg(long, value_name = "POLICY", default_value = "on-ask")]
    approval: String,

    /// Allow a shell command prefix without asking, repeatable (e.g. `--allow-shell-command
    /// "git status"`). Only a *simple* command matches: any shell metacharacter keeps the
    /// call at `ask`. Write these at the subcommand level, never at a bare program name.
    #[arg(long = "allow-shell-command", value_name = "PREFIX")]
    allow_shell_command: Vec<String>,

    /// The model's context window in tokens (ADR 0015). Omitted means the context-budget
    /// guard is **off**: the app does not guess a window, and the status bar says so.
    #[arg(long, value_name = "TOKENS")]
    context_window: Option<u64>,

    #[arg(long)]
    system: Option<String>,

    /// Test hook: enter the terminal, then panic, so the restore hook can be exercised.
    #[arg(long, hide = true)]
    panic_after_enter: bool,

    /// Test hook: start a `sleep` child in its own process group, write its pid to
    /// `<session-dir>/child.pid`, then panic. The pty test asserts the child is not orphaned.
    #[arg(long, hide = true, value_name = "SECS")]
    panic_with_child: Option<u64>,

    /// Test hook: make the driver task panic instead of running, so the UI's watch on it can
    /// be exercised.
    #[arg(long, hide = true)]
    panic_in_driver: bool,
}

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
    // A panic must not unwind through a runtime *drop*: that drop can wait on a parked task
    // and hang. Catch it here, shut the runtime down with a bound (which drops the in-flight
    // tool futures, and with them the `kill_on_drop` child processes they own), then exit
    // 101. The panic hook restores the terminal and returns; it no longer exits, because
    // exiting here would skip exactly that cleanup.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(cli, &runtime)));
    runtime.shutdown_timeout(Duration::from_secs(2));
    match result {
        Ok(Ok(())) => ExitCode::SUCCESS,
        Ok(Err(message)) => {
            eprintln!("radian-tui: {message}");
            ExitCode::from(1)
        }
        Err(_) => ExitCode::from(101),
    }
}

fn run(cli: Cli, runtime: &tokio::runtime::Runtime) -> Result<(), String> {
    if cli.list {
        return list_sessions(&cli);
    }
    let model = build_model(&cli)?;
    let dir = session_dir(&cli)?;
    let workspace = session_workspace(&cli)?;
    let session_id = match (&cli.resume, &cli.session_id) {
        (Some(id), _) | (None, Some(id)) => id.clone(),
        (None, None) => generate_session_id(),
    };
    validate_session_id(&session_id)?;
    let journal = open_journal(&cli, &session_id, &dir, &workspace)?;
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
        // No window configured means the guard is off, never a guessed default (ADR 0015).
        context_budget: cli.context_window.map(ContextBudget::new),
        ..AgentConfig::default()
    };

    // Tools, permissions and approval. The permission layer decides allow/ask/deny; this
    // process is the host that answers `ask` through the UI, and writes every decision to the
    // session journal as a `PermissionResolved` (audit only, ADR 0006 §5.9).
    let tier = radian_tui::profile::parse_tier(&cli.profile)?;
    // The sandbox is built before the tools: for an enforced tier a probe failure is an error
    // here, never a fallback to an unsandboxed run (ADR 0012 decision 2). `shell` only
    // exists when a sandbox does.
    let sandbox = radian_tui::profile::build_sandbox(tier, Path::new(&workspace))?;
    // The description is taken before the sandbox moves into the tools.
    let (startup, meaning) = radian_tui::profile::describe(tier, sandbox.as_ref());
    let tools = build_tools(cli.workspace.as_deref(), sandbox)?;
    let (approval_tx, approval_rx) = std::sync::mpsc::channel::<ApprovalPrompt>();
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
        radian_tui::profile::build_profile(tier, Path::new(&workspace), &cli.allow_shell_command)?
            .with_approval(approval);
    let hooks = Arc::new(PermissionHooks::new(
        profile,
        Some(Arc::new(TuiApprover { tx: approval_tx })),
        Arc::new(JournalAudit {
            journal: Arc::clone(&journal),
            runtime: runtime.handle().clone(),
        }),
    ));
    let agent = Arc::new(
        Agent::new(model, tools)
            .with_config(agent_config)
            .with_hooks(Arc::clone(&hooks)),
    );

    let mut driver = runtime
        .block_on(Driver::open(
            &agent,
            Arc::clone(&journal),
            Arc::clone(&inbox),
            live,
            TurnOptions::default(),
        ))
        .map_err(|e| format!("driver: {e}"))?;

    // The driver runs until the UI asks it to stop; dropping its `run()` future releases the
    // borrow so `shutdown()` can be called.
    // The handle the UI uses for a context-budget "continue": it must be taken before the
    // driver moves onto its task.
    let relax_budget = driver.relax_budget();
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
    app.set_context_window(cli.context_window);
    for message in &history {
        app.push_message(message);
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

    let outcome = ui_loop(
        &mut terminal,
        &mut app,
        &rx,
        &approval_rx,
        runtime,
        &inbox,
        &hooks,
        &driver_task,
        &relax_budget,
    );
    guard.restore();

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
                message.push_str(&format!("\n  {panic}"));
            }
            return Err(message);
        }
    }
    outcome
}

#[allow(clippy::too_many_arguments)]
fn ui_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    rx: &Receiver<AgentEvent>,
    approvals: &Receiver<ApprovalPrompt>,
    runtime: &tokio::runtime::Runtime,
    inbox: &Arc<Inbox>,
    hooks: &Arc<PermissionHooks>,
    driver_task: &tokio::task::JoinHandle<Result<(), radian_runtime::DriverError>>,
    relax_budget: &radian_runtime::RelaxBudget,
) -> Result<(), String> {
    while !app.quit {
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
                        dispatch(send, runtime, inbox, hooks, relax_budget)?;
                    }
                }
                Event::Paste(text) => app.input.push_str(&text),
                _ => {}
            }
        }
        while let Ok(event) = rx.try_recv() {
            app.apply_event(&event);
        }
        // A tool call waiting for the human: show it. The answer goes back over the prompt's
        // own channel when a key is pressed, so this loop never blocks on the decision.
        while let Ok(prompt) = approvals.try_recv() {
            app.request_approval(prompt);
        }
        terminal
            .draw(|frame| radian_tui::render::draw(frame, app))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn dispatch(
    send: Send,
    runtime: &tokio::runtime::Runtime,
    inbox: &Arc<Inbox>,
    hooks: &Arc<PermissionHooks>,
    relax_budget: &radian_runtime::RelaxBudget,
) -> Result<(), String> {
    match send {
        // ADR 0015 C5's "continue": the next turn's first model request passes the guard once,
        // and the guard applies again after it. The turn is opened with a message of the
        // host's own ("continue"), which is what the user chose.
        Send::ContinueBudget => {
            relax_budget.relax_once();
            hooks.set_turn_source(Some(InputSource::User));
            runtime
                .block_on(inbox.followup("continue", InputSource::User))
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        Send::Followup(text) => {
            // `HookContext` has no source field, so the host declares each turn's source
            // before it starts; an unset source is treated as non-human and denied.
            hooks.set_turn_source(Some(InputSource::User));
            runtime
                .block_on(inbox.followup(text, InputSource::User))
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        Send::Steer(text) => {
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
    let home = std::env::var("HOME")
        .map_err(|_| "no HOME to place sessions under; pass --session-dir".to_string())?;
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME")
        && !xdg.is_empty()
    {
        return Ok(PathBuf::from(xdg).join("radian").join("sessions"));
    }
    if cfg!(target_os = "macos") {
        Ok(PathBuf::from(home)
            .join("Library/Application Support")
            .join("radian")
            .join("sessions"))
    } else {
        Ok(PathBuf::from(home)
            .join(".local/share")
            .join("radian")
            .join("sessions"))
    }
}

/// The workspace a session belongs to: `--workspace` when given, else the current directory,
/// canonicalized. `--list` filters by the same value, so a session is only listed in the
/// workspace it was created in.
fn session_workspace(cli: &Cli) -> Result<String, String> {
    let path = match &cli.workspace {
        Some(path) => path.clone(),
        None => std::env::current_dir()
            .map_err(|e| format!("cannot read the current directory: {e}"))?,
    };
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    Ok(path.to_string_lossy().into_owned())
}

/// A per-process identifier, opaque to the server (the same shape `agent-cli` generates).
fn generate_session_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}-{:x}", std::process::id())
}

/// A session id becomes a file name, so it must not be able to escape the session directory
/// or name another file.
fn validate_session_id(id: &str) -> Result<(), String> {
    let ok = !id.is_empty()
        && id.len() <= 128
        && id != "."
        && id != ".."
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "session id {id:?} must be 1-128 characters of [A-Za-z0-9._-]"
        ))
    }
}

/// Opens (or resumes) the session file. The caller has already resolved `session_id`.
fn open_journal(
    cli: &Cli,
    session_id: &str,
    dir: &Path,
    workspace: &str,
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
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    // The snapshot is a model route reference only: never the key, a header value or a base
    // URL that could carry credentials (ADR 0008 §4, ADR 0013).
    let created = SessionCreatedData::new(
        workspace,
        serde_json::json!({"provider": "openai-compatible", "model": cli.model}),
    );
    SessionJournal::create(&path, session_id, created)
        .map(Arc::new)
        .map_err(|e| format!("cannot create session {session_id:?}: {e}"))
}

/// `--list`: this workspace's sessions, newest activity first. Same columns as `agent-cli`.
fn list_sessions(cli: &Cli) -> Result<(), String> {
    let dir = session_dir(cli)?;
    let workspace = session_workspace(cli)?;
    if !dir.exists() {
        return Ok(()); // no sessions yet
    }
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

/// The tool provider the TUI mounts. An enum rather than two code paths so `Agent` keeps one
/// type; `NoTools` is the no-`--workspace` case.
enum TuiTools {
    None(NoTools),
    Coding(Box<CodingTools>),
}

enum TuiSession {
    None(NoTools),
    Coding(Box<CodingSession>),
}

fn build_tools(workspace: Option<&Path>, sandbox: Option<Sandbox>) -> Result<TuiTools, String> {
    match (workspace, sandbox) {
        // With a sandbox, `shell` exists and only ever runs under it (radian-coding refuses to
        // advertise it otherwise). Without one (the `safe` tier), the file tools are still
        // there, confined by their own workspace boundary.
        (Some(dir), Some(sandbox)) => {
            let tools = CodingTools::with_sandbox(dir, sandbox)
                .map_err(|e| format!("--workspace {}: {e}", dir.display()))?;
            Ok(TuiTools::Coding(Box::new(tools)))
        }
        (Some(dir), None) => {
            let tools =
                CodingTools::new(dir).map_err(|e| format!("--workspace {}: {e}", dir.display()))?;
            Ok(TuiTools::Coding(Box::new(tools)))
        }
        (None, _) => Ok(TuiTools::None(NoTools)),
    }
}

impl ToolProvider for TuiTools {
    type Session = TuiSession;

    async fn open_session(&self) -> Result<TuiSession, ToolError> {
        Ok(match self {
            TuiTools::None(tools) => TuiSession::None(tools.open_session().await?),
            TuiTools::Coding(tools) => TuiSession::Coding(Box::new(tools.open_session().await?)),
        })
    }
}

impl ToolSession for TuiSession {
    fn tool_specs(&self) -> &[ToolSpec] {
        match self {
            TuiSession::None(session) => session.tool_specs(),
            TuiSession::Coding(session) => session.tool_specs(),
        }
    }

    async fn execute(
        &self,
        call: &ToolCall,
        cx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolFailure> {
        match self {
            TuiSession::None(session) => session.execute(call, cx).await,
            TuiSession::Coding(session) => session.execute(call, cx).await,
        }
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

fn build_model(cli: &Cli) -> Result<OpenAiCompatible, String> {
    let key = std::env::var(&cli.api_key_env)
        .ok()
        .filter(|k| !k.is_empty());
    let mut config = OpenAiCompatibleConfig::new(&cli.base_url, &cli.model);
    if let Some(key) = key {
        config = config.with_api_key(ApiKey::new(key));
    }
    for header in &cli.header {
        let (name, value) = header
            .split_once('=')
            .ok_or_else(|| format!("--header must be NAME=VALUE: {header:?}"))?;
        config.headers.push((name.to_string(), value.to_string()));
    }
    OpenAiCompatible::new(config).map_err(|e| format!("--base-url {}: {e}", cli.base_url))
}
