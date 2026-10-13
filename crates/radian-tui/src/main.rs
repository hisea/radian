//! The `radian-tui` binary: one real conversation through `radian-runtime`, rendered full
//! screen.
//!
//! Never run this in a shared terminal by hand without expecting it to take over: it enters
//! the alternate screen and raw mode. The tests drive it through a pty.

use std::path::PathBuf;
use std::process::{ExitCode, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use agent_core::{Agent, AgentConfig, AgentEvent, EventSink, InputSource, NoTools, TurnOptions};
use agent_provider_openai_compatible::{ApiKey, OpenAiCompatible, OpenAiCompatibleConfig};
use clap::Parser;
use radian_runtime::{CancelCause, Driver, Inbox};
use radian_session::{SessionCreatedData, SessionJournal};
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

    /// Directory the session JSONL is written to. Created if missing.
    #[arg(long)]
    session_dir: PathBuf,

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
    let model = build_model(&cli)?;
    std::fs::create_dir_all(&cli.session_dir)
        .map_err(|e| format!("--session-dir {}: {e}", cli.session_dir.display()))?;
    let session_id = format!("tui-{}", std::process::id());
    let session_path = cli.session_dir.join("session.jsonl");
    let workspace = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    // SessionCreated is written by the storage layer; the content is the host's.
    let journal = SessionJournal::create(
        &session_path,
        &session_id,
        SessionCreatedData::new(
            workspace,
            serde_json::json!({"model": {"model": cli.model}}),
        ),
    )
    .map_err(|e| format!("session {}: {e}", session_path.display()))?;
    let journal = Arc::new(journal);
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
        std::fs::write(cli.session_dir.join("child.pid"), pid.to_string())
            .map_err(|e| format!("child.pid: {e}"))?;
        panic!("radian-tui: panic path test with a child in flight");
    }
    let mut app = App::new(cli.model.clone(), session_id.clone());
    app.entries.push(radian_tui::Entry::new(
        radian_tui::EntryKind::Notice,
        format!(
            "new session {session_id} · Enter sends · Ctrl-C cancels a turn · Ctrl-C again quits"
        ),
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
