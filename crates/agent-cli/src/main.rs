//! The `agent` command: a chat client wiring the core turn loop to an OpenAI-compatible
//! model and MCP stdio tools, driven by `radian-runtime` so every session is durable.
//!
//! stdout carries only what the model said (streamed); status, prompts, reasoning and
//! errors go to stderr, so `agent -p "..."` can be piped. The API key is read from an
//! environment variable only, and is redacted from everything this program prints — and
//! from everything it writes to disk.

mod mcp;
mod tools;

use std::future::Future;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::io::AsyncBufReadExt;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use agent_core::{
    Agent, AgentConfig, AgentEvent, EventSink, HookContext, HookError, Hooks, InputSource,
    RunStatus, ToolCall, ToolDecision, TurnOptions,
};
use agent_extensions::{McpServerConfig, McpToolProvider};
use agent_provider_openai_compatible::{ApiKey, OpenAiCompatible, OpenAiCompatibleConfig};
use clap::Parser;
use radian_coding::CodingTools;
use radian_runtime::{CancelCause, Driver, DriverError, Inbox, InboxError};
use radian_session::{Index, SessionCreatedData, SessionJournal};
use tokio::sync::{Mutex, mpsc};

use crate::mcp::load_mcp_config;
use crate::tools::CliTools;

/// Exit code for usage and configuration errors, matching clap's own usage failures.
const USAGE_ERROR: i32 = 2;

/// How many inputs may wait in the inbox. Bounded on purpose (ADR 0011 L4): a full queue
/// refuses and hands the message back instead of dropping it.
const INBOX_CAPACITY: usize = 64;

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

    /// Expose the built-in file and read-only git tools rooted at this directory
    /// (read/write/patch/list, git_status/git_diff). No shell is provided.
    #[arg(long)]
    workspace: Option<String>,

    /// One-shot mode: run this prompt, then exit.
    #[arg(short, long, conflicts_with = "list")]
    prompt: Option<String>,

    /// Extra request headers, repeatable: `--header NAME=VALUE`. The value is never
    /// printed; `Authorization` is refused because the key comes only from `--api-key-env`.
    /// A value may contain `{session_id}`, which expands to this process's session id.
    #[arg(long = "header", value_name = "NAME=VALUE")]
    header: Vec<String>,

    /// A stable session identifier for this process: used verbatim as the session file's
    /// name, and available to `--header` values as `{session_id}`. Generated once per
    /// process when omitted.
    #[arg(long, value_name = "VALUE", conflicts_with = "resume")]
    session_id: Option<String>,

    /// Resume an existing session by id.
    #[arg(long, value_name = "SESSION_ID", conflicts_with = "list")]
    resume: Option<String>,

    /// List this workspace's sessions and exit.
    #[arg(long)]
    list: bool,

    /// Where session files live. Overrides `RADIAN_SESSION_DIR` and the user data directory.
    #[arg(long, value_name = "DIR")]
    session_dir: Option<String>,

    /// Ask the server to include usage in the stream. Omit to keep the default policy
    /// (on for `api.openai.com` only, off elsewhere); `--include-usage=false` turns it off.
    #[arg(long, value_name = "BOOL", num_args = 0..=1, default_missing_value = "true")]
    include_usage: Option<bool>,

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
    if cli.list {
        return match list_sessions(&cli) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("agent: {}", e.0);
                USAGE_ERROR
            }
        };
    }
    let setup = match setup(&cli).await {
        Ok(setup) => setup,
        Err(e) => {
            eprintln!("agent: {}", e.0);
            return USAGE_ERROR;
        }
    };
    let redact_secrets = setup.secrets.clone();
    match chat(setup, cli).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("agent: {}", redact(&e.0, &redact_secrets));
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
    /// The session id this process uses: the file name, and the `{session_id}` header value.
    session_id: String,
    /// The key and every custom header value: redacted from all error output, never printed
    /// and never written to the session file.
    secrets: Vec<String>,
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
    // Resolve the session id once per process. `--resume` names an existing session, so it
    // wins; otherwise `--session-id`, otherwise a generated one. The same value is the
    // session file's name and every `{session_id}` in a header.
    let session_id = match (&cli.resume, &cli.session_id) {
        (Some(id), _) | (None, Some(id)) => id.clone(),
        (None, None) => generate_session_id(),
    };
    validate_session_id(&session_id)?;
    let uses_placeholder = cli
        .header
        .iter()
        .any(|item| item.contains(SESSION_PLACEHOLDER));
    let headers = parse_headers(&cli.header, &session_id)?;
    config.headers = headers.clone();
    config.include_usage = cli.include_usage;
    if uses_placeholder && cli.session_id.is_none() && cli.resume.is_none() {
        // Only when the generated id is actually observable: it is what makes the routing
        // reusable across processes.
        eprintln!("[session] {session_id} (pass --session-id to reuse it)");
    }
    let model = OpenAiCompatible::new(config)
        .map_err(|e| SetupError(format!("--base-url {}: {e}", cli.base_url)))?;
    let servers = match &cli.mcp_config {
        Some(path) => load_mcp_config(path).map_err(SetupError)?,
        None => Vec::new(),
    };
    // The key and every header value are secrets: no error this program prints may contain
    // any of them, and they are never written to the session file.
    let mut secrets: Vec<String> = key.into_iter().collect();
    secrets.extend(headers.into_iter().map(|(_, value)| value));
    secrets.retain(|secret| !secret.is_empty());
    Ok(Setup {
        model,
        servers,
        system_prompt,
        max_model_calls: cli.max_model_calls,
        turn_timeout_secs: cli.turn_timeout_secs,
        session_id,
        secrets,
    })
}

/// The placeholder `--header` values may use for the process's session id.
const SESSION_PLACEHOLDER: &str = "{session_id}";

/// A per-process identifier, opaque to the server. Time plus pid is enough for "stable
/// within one process, different across processes"; a UUID dependency would not earn its
/// keep for that.
fn generate_session_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}-{:x}", std::process::id())
}

/// A session id becomes a file name, so it must not be able to escape the session
/// directory or name another file.
fn validate_session_id(id: &str) -> Result<(), SetupError> {
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
        Err(SetupError(format!(
            "session id {id:?} must be 1-128 characters of [A-Za-z0-9._-]"
        )))
    }
}

/// Where session files live: `--session-dir`, then `RADIAN_SESSION_DIR`, then the user's
/// data directory.
///
/// Sessions are durable records, not a cache: ADR 0013 decision 4 says the journal is not
/// subject to cache cleanup, so this is the platform's *data* directory —
/// `$XDG_DATA_HOME/radian/sessions` (defaulting to `~/.local/share/radian/sessions`) on
/// Linux, `~/Library/Application Support/radian/sessions` on macOS. Never a cache or temp
/// path.
fn session_dir(cli: &Cli) -> Result<PathBuf, SetupError> {
    if let Some(dir) = &cli.session_dir {
        return Ok(PathBuf::from(dir));
    }
    if let Ok(dir) = std::env::var("RADIAN_SESSION_DIR")
        && !dir.is_empty()
    {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var("HOME")
        .map_err(|_| SetupError("no HOME to place sessions under; pass --session-dir".into()))?;
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

/// The workspace a session belongs to: `--workspace` when given, else the current
/// directory. `--list` filters by the same value, so a session is only listed in the
/// workspace it was created in.
fn session_workspace(cli: &Cli) -> Result<String, SetupError> {
    let path = match &cli.workspace {
        Some(path) => PathBuf::from(path),
        None => std::env::current_dir()
            .map_err(|e| SetupError(format!("cannot read the current directory: {e}")))?,
    };
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    Ok(path.to_string_lossy().into_owned())
}

/// `--list`: the sessions of this workspace, newest activity first.
fn list_sessions(cli: &Cli) -> Result<(), SetupError> {
    let dir = session_dir(cli)?;
    let workspace = session_workspace(cli)?;
    if !dir.exists() {
        return Ok(()); // no sessions yet
    }
    let mut index = Index::open(dir.join("index.db")).map_err(|e| {
        SetupError(format!(
            "cannot open the session index in {}: {e}",
            dir.display()
        ))
    })?;
    index
        .rebuild_dir(&dir)
        .map_err(|e| SetupError(format!("cannot rebuild the session index: {e}")))?;
    let sessions = index
        .list(Some(&workspace))
        .map_err(|e| SetupError(format!("cannot list sessions: {e}")))?;
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

/// Parses repeated `--header NAME=VALUE` arguments, expanding `{session_id}` in values.
///
/// The value is never echoed, not even in the error for a malformed argument: a user who
/// forgot the `=` may have pasted a credential as the whole argument. The name may be echoed
/// (the brief only forbids values).
fn parse_headers(raw: &[String], session_id: &str) -> Result<Vec<(String, String)>, SetupError> {
    let mut headers = Vec::with_capacity(raw.len());
    for (index, item) in raw.iter().enumerate() {
        let Some((name, value)) = item.split_once('=') else {
            return Err(SetupError(format!(
                "--header (number {}) must be NAME=VALUE",
                index + 1
            )));
        };
        if name.is_empty() {
            return Err(SetupError(format!(
                "--header (number {}) has an empty name",
                index + 1
            )));
        }
        if !is_header_name(name) {
            return Err(SetupError(format!(
                "--header {name}: not a valid HTTP header name"
            )));
        }
        if name.eq_ignore_ascii_case("authorization") {
            return Err(SetupError(
                "--header authorization=... is refused: the API key comes only from \
                 --api-key-env, so a header must not replace it"
                    .into(),
            ));
        }
        let value = value.replace(SESSION_PLACEHOLDER, session_id);
        headers.push((name.to_owned(), value));
    }
    Ok(headers)
}

/// RFC 7230 token characters, which is what a header name may contain.
fn is_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| {
            matches!(b,
                b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.' | b'^'
                    | b'_' | b'`' | b'|' | b'~'
                    | b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z')
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

/// A server may echo the key or a header value back in an error body; nothing this program
/// prints may contain them, so every error goes through here.
fn redact(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_owned();
    for secret in secrets {
        if !secret.is_empty() {
            out = out.replace(secret.as_str(), "<redacted>");
        }
    }
    out
}

/// Opens (or resumes) the session file. The caller has already resolved `session_id`.
fn open_journal(
    cli: &Cli,
    session_id: &str,
    workspace: &str,
) -> Result<Arc<SessionJournal>, SetupError> {
    let dir = session_dir(cli)?;
    let path = dir.join(format!("{session_id}.jsonl"));
    if cli.resume.is_some() {
        if !path.exists() {
            return Err(SetupError(format!(
                "no session {session_id:?} in {}",
                dir.display()
            )));
        }
        let journal = SessionJournal::resume(&path, session_id)
            .map_err(|e| SetupError(format!("cannot resume session {session_id:?}: {e}")))?;
        return Ok(Arc::new(journal));
    }
    std::fs::create_dir_all(&dir)
        .map_err(|e| SetupError(format!("cannot create {}: {e}", dir.display())))?;
    // The snapshot is a model route reference only: never the key, a header value or a
    // base URL that could carry credentials (ADR 0008 §4, ADR 0013).
    let created = SessionCreatedData::new(
        workspace,
        serde_json::json!({"provider": "openai-compatible", "model": cli.model}),
    );
    let journal = SessionJournal::create(&path, session_id, created)
        .map_err(|e| SetupError(format!("cannot create session {session_id:?}: {e}")))?;
    Ok(Arc::new(journal))
}

async fn chat(setup: Setup, cli: Cli) -> Result<i32, SetupError> {
    let interactive = cli.prompt.is_none();
    let lines = LineHub::spawn();
    let can_ask = interactive || std::io::stdin().is_terminal();
    let approval = Approval {
        yes: cli.yes,
        // One-shot mode with no terminal on stdin has nobody to ask; interactive mode
        // reads its answers from the same line hub as the prompt loop.
        can_ask,
        lines: lines.clone(),
    };
    let workspace = session_workspace(&cli)?;
    let journal = open_journal(&cli, &setup.session_id, &workspace)?;
    let agent_config = AgentConfig {
        system_prompt: setup.system_prompt,
        ..AgentConfig::default()
    };
    let coding = match &cli.workspace {
        Some(path) => Some(
            CodingTools::new(path).map_err(|e| SetupError(format!("--workspace {path}: {e}")))?,
        ),
        None => None,
    };
    let agent = Arc::new(
        Agent::new(
            setup.model,
            CliTools::new(coding, McpToolProvider::new(setup.servers)),
        )
        .with_config(agent_config)
        .with_hooks(approval),
    );

    let inbox = Inbox::durable(INBOX_CAPACITY, Arc::clone(&journal));
    let ui = Arc::new(Ui::default());
    let mut signals = spawn_signals();
    // The approval prompt and the prompt loop share stdin. While approval may read, the
    // loop must not compete for a line; with `--yes` (or nobody to ask) it may.
    ui.set_approval_may_read(can_ask && !cli.yes);
    let options = TurnOptions {
        max_model_calls: setup.max_model_calls,
        deadline: None,
        ..TurnOptions::default()
    };
    let mut driver = Driver::open(
        &agent,
        journal,
        Arc::clone(&inbox),
        Arc::clone(&ui) as Arc<dyn EventSink>,
        options,
    )
    .await
    .map_err(|e| SetupError(format!("cannot open the session: {e}")))?;
    // A per-turn timeout: the driver computes a fresh deadline at each turn's start.
    driver.set_turn_timeout(setup.turn_timeout_secs.map(Duration::from_secs));
    let driver_ended = Arc::new(Notify::new());
    let ended = Arc::clone(&driver_ended);
    let mut task = tokio::spawn(async move {
        let result = driver.run().await;
        ended.notify_one();
        result
    });

    let secrets = setup.secrets.clone();
    let code = if let Some(prompt) = cli.prompt {
        one_shot(
            &inbox,
            &ui,
            prompt,
            &driver_ended,
            &mut signals,
            &mut task,
            &secrets,
        )
        .await
    } else {
        interactive_loop(
            &inbox,
            &ui,
            &lines,
            &driver_ended,
            &mut signals,
            &mut task,
            &secrets,
        )
        .await
    };
    ui.end_of_turn();
    Ok(code)
}

/// `-p`: send one input, wait for its turn, then shut the driver down.
async fn one_shot(
    inbox: &Arc<Inbox>,
    ui: &Arc<Ui>,
    prompt: String,
    driver_ended: &Arc<Notify>,
    signals: &mut mpsc::UnboundedReceiver<()>,
    task: &mut JoinHandle<Result<(), DriverError>>,
    secrets: &[String],
) -> i32 {
    if let Err(error) = inbox.followup(prompt, InputSource::User).await {
        eprintln!("agent: {}", redact(&error.to_string(), secrets));
        let _ = shutdown(inbox, ui, task).await;
        return 1;
    }
    let mut cancelling = false;
    match wait_for_turn(ui, inbox, driver_ended, signals, &mut cancelling).await {
        TurnEnd::Forced => return force_exit(task).await,
        TurnEnd::DriverEnded => return report_task(task, secrets).await,
        TurnEnd::Done => {}
    }
    if let Some(code) = report_error(&shutdown(inbox, ui, task).await, secrets) {
        return code;
    }
    match ui.last_status() {
        Some(RunStatus::Completed) => 0,
        Some(status) => {
            eprintln!("agent: the turn ended as {status:?}");
            1
        }
        None => 1,
    }
}

/// Interactive: read lines and route them into the inbox.
async fn interactive_loop(
    inbox: &Arc<Inbox>,
    ui: &Arc<Ui>,
    lines: &LineHub,
    driver_ended: &Arc<Notify>,
    signals: &mut mpsc::UnboundedReceiver<()>,
    task: &mut JoinHandle<Result<(), DriverError>>,
    secrets: &[String],
) -> i32 {
    // True once the first Ctrl-C of a wind-down has been seen: the second one forces exit.
    let mut cancelling = false;
    loop {
        if !ui.busy() {
            cancelling = false;
            eprint!("> ");
            let _ = std::io::stderr().flush();
        }
        match next_event(lines, ui, inbox, driver_ended, signals, &mut cancelling).await {
            Next::Leave => break,
            Next::Forced => return force_exit(task).await,
            Next::DriverEnded => return report_task(task, secrets).await,
            Next::Again => continue,
            Next::Line(line) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if line == "/exit" || line == "/quit" {
                    break;
                }
                // A turn in flight gets the line as a steer (it changes this turn's
                // direction); an idle driver gets a followup (a new turn). Both are
                // `source = User`.
                let result = if ui.busy() {
                    inbox.steer(line.to_owned(), InputSource::User).await
                } else {
                    inbox.followup(line.to_owned(), InputSource::User).await
                };
                if let Err(error) = result {
                    match &error {
                        InboxError::Full { .. } => eprintln!(
                            "agent: the inbox is full; that input was not queued (try again)"
                        ),
                        other => eprintln!("agent: {}", redact(&other.to_string(), secrets)),
                    }
                    continue;
                }
                if ui.approval_may_read() {
                    // Do not read again until the turn has started: an approval answer the
                    // user types now must reach the approval prompt, not become the next
                    // input.
                    tokio::select! {
                        _ = ui.started.notified() => {}
                        _ = ui.finished.notified() => {}
                    }
                }
            }
        }
    }
    if let Some(code) = report_error(&shutdown(inbox, ui, task).await, secrets) {
        return code;
    }
    0
}

/// What the interactive loop is waiting for.
enum Next {
    Line(String),
    /// A turn ended, or a cancellation was requested: re-evaluate and keep going.
    Again,
    /// Leave (EOF, `/exit`, or Ctrl-C at the prompt).
    Leave,
    /// A second Ctrl-C during a wind-down.
    Forced,
    /// The driver stopped on its own (an error).
    DriverEnded,
}

/// Waits for the next thing that can happen: a line, a turn ending, the driver ending, or a
/// signal. While a turn is in flight and the approval prompt may read stdin, the loop does
/// **not** read — the approval owns stdin then, exactly as before this slice.
async fn next_event(
    lines: &LineHub,
    ui: &Ui,
    inbox: &Inbox,
    driver_ended: &Arc<Notify>,
    signals: &mut mpsc::UnboundedReceiver<()>,
    cancelling: &mut bool,
) -> Next {
    let read_lines = !ui.busy() || !ui.approval_may_read();
    // `biased` with the signal first: a pending Ctrl-C must beat a ready EOF or a turn that
    // just ended, or a forced exit could be skipped in favour of a graceful one.
    tokio::select! {
        biased;
        _ = signals.recv() => on_signal(ui, inbox, cancelling),
        _ = driver_ended.notified() => Next::DriverEnded,
        _ = ui.finished.notified(), if ui.busy() => Next::Again,
        line = lines.next(), if read_lines => match line {
            Some(line) => Next::Line(line),
            None => Next::Leave,
        },
    }
}

fn on_signal(ui: &Ui, inbox: &Inbox, cancelling: &mut bool) -> Next {
    // The second Ctrl-C of an open wind-down always forces, even if the turn ended in the
    // meantime: the user asked twice for it.
    if *cancelling {
        return Next::Forced;
    }
    if !ui.busy() {
        // At the prompt, Ctrl-C means leave, like EOF.
        return Next::Leave;
    }
    *cancelling = true;
    inbox.cancel(CancelCause::Requested);
    eprintln!("agent: cancelling (Ctrl-C again to quit)");
    Next::Again
}

/// Stops the driver gracefully: it finishes the turns already queued, closes the session
/// (so MCP servers shut down), and returns.
async fn shutdown(
    inbox: &Arc<Inbox>,
    ui: &Arc<Ui>,
    task: &mut JoinHandle<Result<(), DriverError>>,
) -> Result<(), String> {
    // Let queued inputs run: closing now would abandon them.
    while ui.busy() || !inbox.is_empty().await {
        ui.finished.notified().await;
    }
    inbox.close();
    match task.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(error.to_string()),
        Err(join) => Err(format!("the driver task ended unexpectedly: {join}")),
    }
}

/// Aborts the driver: the session is dropped, which kills the tool processes at once. Used
/// only for the forced second Ctrl-C.
async fn force_exit(task: &mut JoinHandle<Result<(), DriverError>>) -> i32 {
    task.abort();
    let _ = task.await;
    130
}

/// Awaits the driver task once and reports its error, if any.
async fn report_task(task: &mut JoinHandle<Result<(), DriverError>>, secrets: &[String]) -> i32 {
    match task.await {
        Ok(Ok(())) => 0,
        Ok(Err(error)) => {
            eprintln!("agent: {}", redact(&error.to_string(), secrets));
            1
        }
        Err(join) => {
            eprintln!("agent: the driver task ended unexpectedly: {join}");
            1
        }
    }
}

fn report_error(result: &Result<(), String>, secrets: &[String]) -> Option<i32> {
    match result {
        Ok(()) => None,
        Err(error) => {
            eprintln!("agent: {}", redact(error, secrets));
            Some(1)
        }
    }
}

/// Waits for the turn in flight to end; the first Ctrl-C cancels it, the second forces exit.
async fn wait_for_turn(
    ui: &Ui,
    inbox: &Inbox,
    driver_ended: &Arc<Notify>,
    signals: &mut mpsc::UnboundedReceiver<()>,
    cancelling: &mut bool,
) -> TurnEnd {
    loop {
        tokio::select! {
            biased;
            _ = signals.recv() => {
                if *cancelling {
                    return TurnEnd::Forced;
                }
                if !ui.busy() {
                    return TurnEnd::Done;
                }
                *cancelling = true;
                inbox.cancel(CancelCause::Requested);
                eprintln!("agent: cancelling (Ctrl-C again to quit)");
            }
            _ = driver_ended.notified() => return TurnEnd::DriverEnded,
            _ = ui.finished.notified() => return TurnEnd::Done,
        }
    }
}

enum TurnEnd {
    Done,
    /// A second Ctrl-C arrived while the first cancellation was still settling.
    Forced,
    /// The driver stopped before the turn finished (an error).
    DriverEnded,
}

/// One long-lived SIGINT listener.
///
/// `tokio::signal::ctrl_c()` does **not** buffer: every call creates a fresh watch receiver,
/// so a signal that arrives while no `ctrl_c()` future is registered is lost. Awaiting a
/// fresh `ctrl_c()` on each loop iteration (as this CLI first did) therefore dropped the
/// second Ctrl-C of a wind-down now and then, and the CLI left with code 0 instead of 130.
/// One task loops on `ctrl_c()` and forwards into a channel, so no signal can fall between
/// two calls.
fn spawn_signals() -> mpsc::UnboundedReceiver<()> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                // The handler could not be installed (it can be retried); do not drop the
                // channel, or the loop would spin on a closed receiver.
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
            if tx.send(()).is_err() {
                break;
            }
        }
    });
    rx
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

/// Streams model text to stdout; everything else — status, reasoning, tool lines — goes to
/// stderr. stdout stays free of status output so one-shot mode can be piped.
///
/// Reasoning is rendered on stderr with a `[reasoning]` prefix, on its own line, so it is
/// never confused with the visible answer (ADR 0014: reasoning is a separate channel).
#[derive(Default)]
struct Ui {
    streamed: AtomicBool,
    reasoning_open: AtomicBool,
    busy: AtomicBool,
    /// Resolves when a turn starts.
    started: Notify,
    /// Resolves when a turn ends.
    finished: Notify,
    /// True while the approval prompt may read stdin (so the prompt loop must not).
    approval_may_read: AtomicBool,
    last_status: StdMutex<Option<RunStatus>>,
}

impl Ui {
    /// True while a turn is in flight.
    fn busy(&self) -> bool {
        self.busy.load(Ordering::Relaxed)
    }

    fn set_approval_may_read(&self, yes: bool) {
        self.approval_may_read.store(yes, Ordering::Relaxed);
    }

    fn approval_may_read(&self) -> bool {
        self.approval_may_read.load(Ordering::Relaxed)
    }

    fn last_status(&self) -> Option<RunStatus> {
        self.last_status.lock().unwrap().clone()
    }

    /// Ends the streamed line, if anything was streamed this turn.
    fn end_of_turn(&self) {
        if self.reasoning_open.swap(false, Ordering::Relaxed) {
            eprintln!();
        }
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
            AgentEvent::TurnStarted => {
                self.busy.store(true, Ordering::Relaxed);
                self.started.notify_one();
            }
            AgentEvent::ModelReasoningDelta { text } => {
                if !self.reasoning_open.swap(true, Ordering::Relaxed) {
                    eprint!("[reasoning] ");
                }
                eprint!("{text}");
                let _ = std::io::stderr().flush();
            }
            AgentEvent::ModelTextDelta { text } => {
                if self.reasoning_open.swap(false, Ordering::Relaxed) {
                    eprintln!();
                }
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
            AgentEvent::RunFinished { status } => {
                if self.reasoning_open.swap(false, Ordering::Relaxed) {
                    eprintln!();
                }
                self.busy.store(false, Ordering::Relaxed);
                // End this turn's streamed line (and close a reasoning line) so the next
                // turn's output starts on its own line.
                self.end_of_turn();
                *self.last_status.lock().unwrap() = Some(status.clone());
                match &status {
                    RunStatus::Completed => {}
                    RunStatus::Cancelled(_) => {
                        // Carries both the lowercase hint and the debug status: the tests
                        // wait for either.
                        eprintln!("agent: cancelled ({status:?}); type another line or /exit")
                    }
                    other => eprintln!("[turn] ended as {other:?}"),
                }
                self.finished.notify_one();
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
