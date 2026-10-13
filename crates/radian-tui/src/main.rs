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

use agent_core::{Agent, AgentConfig, AgentEvent, EventSink, InputSource, NoTools, TurnOptions};
use agent_provider_openai_compatible::{ApiKey, OpenAiCompatible, OpenAiCompatibleConfig};
use clap::Parser;
use radian_runtime::{CancelCause, Driver, Inbox};
use radian_session::{Index, SessionCreatedData, SessionJournal};
use radian_tui::app::{App, Send};
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

    #[arg(long)]
    system: Option<String>,

    /// Test hook: enter the terminal, then panic, so the restore hook can be exercised.
    #[arg(long, hide = true)]
    panic_after_enter: bool,

    /// Test hook: start a `sleep` child in its own process group, write its pid to
    /// `<session-dir>/child.pid`, then panic. The pty test asserts the child is not orphaned.
    #[arg(long, hide = true, value_name = "SECS")]
    panic_with_child: Option<u64>,
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
        ..AgentConfig::default()
    };
    let agent = Arc::new(Agent::new(model, NoTools).with_config(agent_config));

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
    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
    let driver_task = runtime.spawn(async move {
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
    for message in &history {
        app.push_message(message);
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

    let outcome = ui_loop(&mut terminal, &mut app, &rx, runtime, &inbox);
    guard.restore();

    let _ = stop_tx.send(());
    let _ = runtime.block_on(driver_task);
    outcome
}

fn ui_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    rx: &Receiver<AgentEvent>,
    runtime: &tokio::runtime::Runtime,
    inbox: &Arc<Inbox>,
) -> Result<(), String> {
    while !app.quit {
        // A short poll keeps rendering responsive without blocking on input; it is a UI
        // refresh interval, not a synchronization device.
        if event::poll(Duration::from_millis(50)).map_err(|e| e.to_string())? {
            match event::read().map_err(|e| e.to_string())? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    if let Some(send) = app.on_key(key) {
                        dispatch(send, runtime, inbox)?;
                    }
                }
                Event::Paste(text) => app.input.push_str(&text),
                _ => {}
            }
        }
        while let Ok(event) = rx.try_recv() {
            app.apply_event(&event);
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
) -> Result<(), String> {
    match send {
        Send::Followup(text) => runtime
            .block_on(inbox.followup(text, InputSource::User))
            .map(|_| ())
            .map_err(|e| e.to_string()),
        Send::Steer(text) => runtime
            .block_on(inbox.steer(text, InputSource::User))
            .map(|_| ())
            .map_err(|e| e.to_string()),
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
