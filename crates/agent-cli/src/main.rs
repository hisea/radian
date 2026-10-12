//! The `agent` command: a chat client wiring the core turn loop to an OpenAI-compatible
//! model and MCP stdio tools.
//!
//! stdout carries only what the model said (streamed); status, prompts and errors go to
//! stderr, so `agent -p "..."` can be piped. The API key is read from an environment
//! variable only, and is redacted from everything this program prints.

mod mcp;

use std::future::Future;
use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::io::AsyncBufReadExt;
use tokio::time::Instant;

use agent_core::{
    Agent, AgentConfig, AgentEvent, AgentSession, EventSink, HookContext, HookError, Hooks,
    RunError, RunOutcome, RunStatus, ToolCall, ToolDecision, TurnOptions,
};
use agent_extensions::{McpServerConfig, McpToolProvider};
use agent_provider_openai_compatible::{ApiKey, OpenAiCompatible, OpenAiCompatibleConfig};
use clap::Parser;
use tokio::sync::{Mutex, mpsc};

use crate::mcp::load_mcp_config;

/// Exit code for usage and configuration errors, matching clap's own usage failures.
const USAGE_ERROR: i32 = 2;

#[derive(Parser)]
#[command(name = "agent", about = "Chat with a model, with optional MCP tools")]
struct Cli {
    /// Model name sent to the API.
    #[arg(long)]
    model: String,

    /// Everything before `/chat/completions`.
    #[arg(long, default_value = "https://api.openai.com/v1")]
    base_url: String,

    /// Environment variable holding the API key. The key is never read from anywhere else
    /// and never appears on the command line.
    #[arg(long, default_value = "OPENAI_API_KEY")]
    api_key_env: String,

    /// System prompt text.
    #[arg(long, conflicts_with = "system_file")]
    system: Option<String>,

    /// File containing the system prompt.
    #[arg(long)]
    system_file: Option<String>,

    /// MCP server config file (JSON).
    #[arg(long)]
    mcp_config: Option<String>,

    /// One-shot mode: run this prompt, then exit.
    #[arg(short, long)]
    prompt: Option<String>,

    /// Approve every tool call without asking.
    #[arg(long)]
    yes: bool,

    /// Override the per-turn model call limit.
    #[arg(long)]
    max_model_calls: Option<u32>,

    /// Per-turn time limit in seconds.
    #[arg(long)]
    turn_timeout_secs: Option<u64>,
}

/// Why startup failed. The message must not contain the key.
struct SetupError(String);

fn main() {
    let cli = Cli::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let code = runtime.block_on(run(cli));
    // The session is closed on every path inside `run`. Dropping a runtime waits for its
    // blocking threads, and the stdin reader's blocking read cannot be interrupted: with stdin
    // still open it would hold the process here until the user typed another line.
    runtime.shutdown_background();
    std::process::exit(code);
}

async fn run(cli: Cli) -> i32 {
    let setup = match setup(&cli).await {
        Ok(setup) => setup,
        Err(e) => {
            eprintln!("agent: {}", e.0);
            return USAGE_ERROR;
        }
    };
    let redact_key = setup.key.clone();
    match chat(setup, cli).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("agent: {}", redact(&e.0, redact_key.as_deref()));
            USAGE_ERROR
        }
    }
}

struct Setup {
    model: OpenAiCompatible,
    servers: Vec<McpServerConfig>,
    system_prompt: Option<String>,
    max_model_calls: Option<u32>,
    turn_timeout_secs: Option<u64>,
    /// Kept only to redact the key from error output, never printed.
    key: Option<String>,
}

async fn setup(cli: &Cli) -> Result<Setup, SetupError> {
    let system_prompt = match (&cli.system, &cli.system_file) {
        (Some(text), _) => Some(text.clone()),
        (None, Some(path)) => Some(
            std::fs::read_to_string(path)
                .map_err(|e| SetupError(format!("cannot read --system-file {path}: {e}")))?,
        ),
        _ => None,
    };
    let key = std::env::var(&cli.api_key_env)
        .ok()
        .filter(|k| !k.is_empty());
    if key.is_none() && !is_local_base_url(&cli.base_url) {
        return Err(SetupError(format!(
            "no API key: set ${} (or point --api-key-env at the right variable); \
             refusing to continue because {} is not a local address",
            cli.api_key_env, cli.base_url
        )));
    }
    let mut config = OpenAiCompatibleConfig::new(&cli.base_url, &cli.model);
    if let Some(key) = &key {
        config = config.with_api_key(ApiKey::new(key.clone()));
    }
    let model = OpenAiCompatible::new(config)
        .map_err(|e| SetupError(format!("--base-url {}: {e}", cli.base_url)))?;
    let servers = match &cli.mcp_config {
        Some(path) => load_mcp_config(path).map_err(SetupError)?,
        None => Vec::new(),
    };
    Ok(Setup {
        model,
        servers,
        system_prompt,
        max_model_calls: cli.max_model_calls,
        turn_timeout_secs: cli.turn_timeout_secs,
        key,
    })
}

/// Like the provider's own rule: localhost, 127.0.0.1 and ::1 may go without a key.
fn is_local_base_url(base_url: &str) -> bool {
    let Some((_, rest)) = base_url.split_once("://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.strip_prefix('[').map_or_else(
        || {
            authority
                .rsplit_once(':')
                .filter(|(_, port)| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()))
                .map_or(authority, |(host, _)| host)
        },
        |bracketed| bracketed.split(']').next().unwrap_or_default(),
    );
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
}

/// A server may echo the key back in an error body; nothing this program prints may
/// contain it, so every error goes through here.
fn redact(text: &str, key: Option<&str>) -> String {
    match key {
        Some(key) if !key.is_empty() => text.replace(key, "<redacted>"),
        _ => text.to_owned(),
    }
}

async fn chat(setup: Setup, cli: Cli) -> Result<i32, SetupError> {
    let interactive = cli.prompt.is_none();
    let lines = LineHub::spawn();
    let approval = Approval {
        yes: cli.yes,
        // One-shot mode with no terminal on stdin has nobody to ask; interactive mode
        // reads its answers from the same line hub as the prompt loop.
        can_ask: interactive || std::io::stdin().is_terminal(),
        lines: lines.clone(),
    };
    let agent_config = AgentConfig {
        system_prompt: setup.system_prompt,
        ..AgentConfig::default()
    };
    let agent = Arc::new(
        Agent::new(setup.model, McpToolProvider::new(setup.servers))
            .with_config(agent_config)
            .with_hooks(approval),
    );
    let mut session = match agent.open_session().await {
        Ok(session) => session,
        Err(e) => return Err(SetupError(format!("cannot start tools: {e}"))),
    };
    let ui = Ui::default();
    let key = setup.key.as_deref();
    let turn_options = |deadline: Option<Instant>| TurnOptions {
        max_model_calls: setup.max_model_calls,
        deadline,
        ..TurnOptions::default()
    };
    let deadline = |secs: u64| Instant::now() + Duration::from_secs(secs);

    let code = if let Some(prompt) = cli.prompt {
        let options = turn_options(setup.turn_timeout_secs.map(deadline));
        match run_turn(&mut session, prompt, options, &ui).await {
            TurnEnd::Forced => 130,
            TurnEnd::Done(Ok(outcome)) => match outcome.status {
                RunStatus::Completed => 0,
                status => {
                    eprintln!("agent: the turn ended as {status:?}");
                    1
                }
            },
            TurnEnd::Done(Err(e)) => {
                eprintln!("agent: {}", redact(&e.to_string(), key));
                1
            }
        }
    } else {
        interactive_loop(
            &mut session,
            &lines,
            &ui,
            &turn_options,
            setup.turn_timeout_secs,
            key,
        )
        .await
    };
    // Every exit path closes the session first, so each MCP server sees its stdin close,
    // gets the chance to shut down on its own, and is reaped before the process goes away.
    // A forced exit (a second Ctrl-C) skips the wait: dropping the session still kills the
    // servers' process groups at once.
    if code != 130
        && let Err(e) = session.close().await
    {
        eprintln!("agent: could not close the tools cleanly: {e}");
    }
    ui.end_of_turn();
    Ok(code)
}

/// Reads stdin on one dedicated task and hands each line to whoever needs it next: the
/// interactive prompt loop between turns, tool approval during one. A blocking read cannot
/// be cancelled, so approval must never read stdin directly — otherwise a cancelled turn
/// would leave a read behind that swallows the user's next line.
#[derive(Clone)]
struct LineHub {
    rx: Arc<Mutex<mpsc::Receiver<String>>>,
}

impl LineHub {
    fn spawn() -> Self {
        let (tx, rx) = mpsc::channel(4);
        tokio::spawn(async move {
            let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
            let mut line = String::new();
            loop {
                line.clear();
                match stdin.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let trimmed = line.trim_end_matches(['\r', '\n']).to_owned();
                        if tx.send(trimmed).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Self {
            rx: Arc::new(Mutex::new(rx)),
        }
    }

    async fn next(&self) -> Option<String> {
        self.rx.lock().await.recv().await
    }
}

// `RunOutcome` deliberately owns its transcript; avoid changing this local control-flow enum's
// simple shape merely because host metadata may include opaque JSON.
#[allow(clippy::large_enum_variant)]
enum TurnEnd {
    Done(Result<RunOutcome, RunError>),
    /// A second Ctrl-C arrived while the first cancellation was still settling.
    Forced,
}

/// Runs one turn. The first Ctrl-C cancels through `CancellationHandle` while the future
/// is kept alive (dropping it would poison the session); a second Ctrl-C during the
/// wind-down drops it on purpose — the exit that follows is the one supported use of that.
async fn run_turn(
    session: &mut AgentSession<OpenAiCompatible, McpToolProvider, Approval>,
    input: String,
    options: TurnOptions,
    ui: &Ui,
) -> TurnEnd {
    let mut run = match session.start_turn_with(input, options) {
        Ok(run) => run,
        Err(e) => return TurnEnd::Done(Err(RunError::Session(e))),
    };
    let cancel = run.cancellation_handle();
    let (force_tx, mut force_rx) = mpsc::channel::<()>(1);
    let interrupt = tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        cancel.cancel();
        // A second Ctrl-C while the cancellation is still settling: stop waiting.
        let _ = tokio::signal::ctrl_c().await;
        let _ = force_tx.send(()).await;
    });
    let ended = tokio::select! {
        outcome = run.run(ui) => TurnEnd::Done(outcome),
        _ = force_rx.recv() => TurnEnd::Forced,
    };
    interrupt.abort();
    ended
}

async fn interactive_loop(
    session: &mut AgentSession<OpenAiCompatible, McpToolProvider, Approval>,
    lines: &LineHub,
    ui: &Ui,
    turn_options: &impl Fn(Option<Instant>) -> TurnOptions,
    timeout_secs: Option<u64>,
    key: Option<&str>,
) -> i32 {
    loop {
        eprint!("> ");
        let _ = std::io::stderr().flush();
        let line = tokio::select! {
            line = lines.next() => match line {
                Some(line) => line,
                None => return 0, // EOF
            },
            // At the prompt, Ctrl-C means leave, like EOF.
            _ = tokio::signal::ctrl_c() => return 0,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line == "/exit" || line == "/quit" {
            return 0;
        }
        // Fresh per turn: a deadline computed at startup would already be spent on turn two.
        let deadline = timeout_secs.map(|secs| Instant::now() + Duration::from_secs(secs));
        match run_turn(session, line.to_owned(), turn_options(deadline), ui).await {
            TurnEnd::Forced => return 130,
            TurnEnd::Done(Ok(outcome)) => match outcome.status {
                RunStatus::Completed => {}
                RunStatus::Cancelled(_) => {
                    eprintln!("agent: cancelled; type another line or /exit")
                }
                status => eprintln!("agent: the turn ended as {status:?}"),
            },
            TurnEnd::Done(Err(e)) => eprintln!("agent: {}", redact(&e.to_string(), key)),
        }
        ui.end_of_turn();
    }
}

/// Streams model text to stdout; everything else goes to stderr. stdout stays free of
/// status output so one-shot mode can be piped.
#[derive(Default)]
struct Ui {
    streamed: AtomicBool,
}

impl Ui {
    /// Ends the streamed line, if anything was streamed this turn.
    fn end_of_turn(&self) {
        if self.streamed.swap(false, Ordering::Relaxed) {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(b"\n");
            let _ = out.flush();
        }
    }
}

impl EventSink for Ui {
    fn emit(&self, event: AgentEvent) {
        match event {
            AgentEvent::ModelTextDelta { text } => {
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(text.as_bytes());
                let _ = out.flush();
                self.streamed.store(true, Ordering::Relaxed);
            }
            AgentEvent::ToolStarted { index, name, .. } => {
                eprintln!("[tool {index}] {name}: started");
            }
            AgentEvent::ToolFinished {
                index, is_error, ..
            } => {
                if is_error {
                    eprintln!("[tool {index}] finished with an error");
                } else {
                    eprintln!("[tool {index}] finished");
                }
            }
            AgentEvent::RunFinished { status } if status != RunStatus::Completed => {
                eprintln!("[turn] ended as {status:?}");
            }
            _ => {}
        }
    }
}

/// Asks the user about each tool call on stderr, reading the answer from the shared line
/// hub. With `--yes`, or with nobody to ask, every call is allowed or denied without
/// waiting, so the turn can never hang on approval.
struct Approval {
    yes: bool,
    can_ask: bool,
    lines: LineHub,
}

impl Hooks for Approval {
    fn before_tool_call(
        &self,
        _cx: &HookContext,
        call: &ToolCall,
    ) -> impl Future<Output = Result<ToolDecision, HookError>> + Send {
        let lines = self.lines.clone();
        let (yes, can_ask) = (self.yes, self.can_ask);
        async move {
            if yes {
                return Ok(ToolDecision::Allow);
            }
            if !can_ask {
                return Ok(ToolDecision::Deny {
                    reason: "not approved: this run has no terminal to ask in; \
                             pass --yes to allow tool calls"
                        .into(),
                });
            }
            eprint!(
                "[approve] tool {} args {}\nrun it? [y/N] ",
                call.name, call.arguments
            );
            let _ = std::io::stderr().flush();
            // No cancellation check is needed here: the core drops this future as soon as the
            // turn is cancelled, and a line leaves the hub only when `next()` completes, so a
            // cancelled approval cannot swallow the user's next line.
            match lines.next().await.as_deref() {
                Some("y") | Some("Y") => Ok(ToolDecision::Allow),
                Some(other) => Ok(ToolDecision::Deny {
                    reason: format!(
                        "the user denied this call (answer: {other:?}; only `y` approves)"
                    ),
                }),
                None => Ok(ToolDecision::Deny {
                    reason: "no input left to approve with; pass --yes".into(),
                }),
            }
        }
    }
}
