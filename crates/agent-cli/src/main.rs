//! The `agent` command: a chat client wiring the core turn loop to an OpenAI-compatible
//! model and MCP stdio tools, driven by `radian-runtime` so every session is durable.
//!
//! stdout carries only what the model said (streamed); status, prompts, reasoning and
//! errors go to stderr, so `agent -p "..."` can be piped. The API key is read from an
//! environment variable only, and is redacted from everything this program prints — and
//! from everything it writes to disk.

mod approval;
mod profile;
mod tools;

use std::collections::HashMap;
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
    Agent, AgentConfig, AgentEvent, BudgetReport, ContextBudget, EventSink, InputSource, RunStatus,
    TurnOptions, UsageSource,
};
use agent_extensions::{McpServerConfig, McpToolProvider};
use agent_provider_openai_compatible::{ApiKey, OpenAiCompatible, OpenAiCompatibleConfig};
use clap::Parser;
use radian_coding::CodingTools;
use radian_host::{
    SESSION_PLACEHOLDER, generate_session_id, load_mcp_config, parse_headers, resolve_workspace,
    validate_session_id,
};
use radian_permissions::{Approver, PermissionHooks};
use radian_runtime::{BudgetRelax, CancelCause, Driver, DriverError, Inbox, InboxError};
use radian_session::{
    EventData, ExportOptions, Index, Session, SessionCreatedData, SessionJournal, export_json,
    export_markdown,
};
use tokio::sync::{Mutex, mpsc};

use crate::approval::{AutoApprover, JournalAudit, TERMINAL_SOURCE, TerminalApprover};
use crate::tools::CliTools;

/// Exit code for usage and configuration errors, matching clap's own usage failures.
const USAGE_ERROR: i32 = 2;

/// The exit code for "the context-budget guard stopped the turn in one-shot mode". Distinct
/// from 1 (the turn failed), 2 (usage/setup) and 130 (forced exit) so a script can tell a
/// budget stop from an error. One-shot mode never continues automatically.
const CONTEXT_BUDGET_EXIT: i32 = 3;

/// What the CLI sends when the user chooses to continue after a context-budget stop. The user
/// chose to continue, and a user message is the only way to open a new turn (Core's
/// `start_turn` requires one); it records that choice truthfully. The transcript itself is
/// never edited (ADR 0015 C7).
const CONTINUE_MESSAGE: &str = "continue";

/// How many inputs may wait in the inbox. Bounded on purpose (ADR 0011 L4): a full queue
/// refuses and hands the message back instead of dropping it.
const INBOX_CAPACITY: usize = 64;

#[derive(Parser)]
#[command(name = "agent", about = "Chat with a model, with optional MCP tools")]
struct Cli {
    /// Model name sent to the API. Required to run a conversation (`-p` or interactive); the
    /// session-management commands (`--list`, `--export`, `--search`) do not need a model and
    /// do not read the key.
    #[arg(
        long,
        required_unless_present_any = ["list", "export", "search"]
    )]
    model: Option<String>,

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
    /// (read/write/patch/list, git_status/git_diff). Under `workspace-write` this also
    /// exposes `shell`, which runs only inside the sandbox.
    #[arg(long)]
    workspace: Option<String>,

    /// Permission profile: `safe` (read-only, no shell, no MCP), `workspace-write` (the
    /// default: writes confined to the workspace, `shell` sandboxed), or `full-access`
    /// (explicitly unsandboxed; never a default).
    #[arg(long, value_name = "PROFILE")]
    profile: Option<profile::ProfileArg>,

    /// Allow a simple shell command prefix without asking (repeatable), e.g.
    /// `--allow-shell-command "git status"`. Only a simple command matches; a composite
    /// (`git status; rm -x`) still asks. There is no allow rule for shell by default.
    #[arg(long = "allow-shell-command", value_name = "PREFIX")]
    allow_shell_command: Vec<String>,

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

    /// Export one session of this workspace and exit. Markdown by default; `--format json`
    /// for the raw event document. Only the current workspace's sessions can be exported.
    #[arg(
        long,
        value_name = "SESSION_ID",
        conflicts_with_all = ["prompt", "resume", "list", "search"]
    )]
    export: Option<String>,

    /// Search this workspace's sessions for message text and exit. Prints one
    /// `session-id`/time/one-line-snippet row per hit; no match exits 0 with a note on stderr.
    #[arg(
        long,
        value_name = "TEXT",
        conflicts_with_all = ["prompt", "resume", "list", "export"]
    )]
    search: Option<String>,

    /// Export format: `markdown` (the readable transcript) or `json` (the event document).
    #[arg(long, value_name = "FORMAT", value_enum, default_value_t = ExportFormat::Markdown, requires = "export")]
    format: ExportFormat,

    /// Write the export here instead of stdout. Refuses to overwrite unless `--force`.
    #[arg(long, value_name = "FILE", requires = "export")]
    out: Option<PathBuf>,

    /// Overwrite `--out` if it exists.
    #[arg(long, requires = "out")]
    force: bool,

    /// Include the assistant reasoning in the export. Off by default (ADR 0014).
    #[arg(long, requires = "export")]
    include_reasoning: bool,

    /// Include the raw provider payloads in the export. Off by default (ADR 0013).
    #[arg(long, requires = "export")]
    include_provider_payload: bool,

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

    /// The model's context window in tokens. Omit to leave the context-budget guard disabled:
    /// no window is guessed, because a model-name-to-window table ages and is mostly
    /// remembered numbers (ADR 0015 C1).
    #[arg(long, value_name = "TOKENS")]
    context_window: Option<u64>,

    /// Block when the projected request size reaches this percent of the window (default 80).
    #[arg(long, value_name = "PERCENT", default_value_t = agent_core::DEFAULT_THRESHOLD_PERCENT)]
    context_threshold: u8,
}

/// Why startup failed. The message must not contain the key.
pub(crate) struct SetupError(pub(crate) String);

/// The `--model` value. Clap requires it unless `--list`, `--export` or `--search` is present,
/// so a conversation always has one; this makes the failure explicit instead of a panic if
/// that invariant ever changes.
fn model_name(cli: &Cli) -> Result<&str, SetupError> {
    cli.model
        .as_deref()
        .ok_or_else(|| SetupError("--model is required to run a conversation".into()))
}

/// The `--format` value for `--export`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum ExportFormat {
    Markdown,
    Json,
}

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
    // The session-management commands never touch the model or the network: they run before
    // `setup` (so no key is required) and never create a session file.
    if let Some(session_id) = &cli.export {
        return match export_session(&cli, session_id) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("agent: {}", e.0);
                USAGE_ERROR
            }
        };
    }
    if let Some(query) = &cli.search {
        return match search_sessions(&cli, query) {
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
    /// The context-budget guard, or `None` when no window was configured (the guard is then
    /// disabled, not given a guessed default).
    context_budget: Option<ContextBudget>,
    /// The session id this process uses: the file name, and the `{session_id}` header value.
    session_id: String,
    /// The validated, canonical workspace (dogfooding D1). Resolved in `setup`, before any
    /// session file exists, so a bad `--workspace` fails as a workspace error and never
    /// reaches the sandbox probe or leaves a session behind (dogfooding D2).
    workspace: PathBuf,
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
    let mut config = OpenAiCompatibleConfig::new(&cli.base_url, model_name(cli)?);
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
    validate_session_id(&session_id).map_err(SetupError)?;
    // D1: the workspace is validated here, before the sandbox is built and before the session
    // file is created, so a missing or unusable path is a workspace error (not a sandbox
    // message) and leaves nothing behind (D2). `radian-host` owns the rule for both hosts.
    let workspace = resolve_workspace(cli.workspace.as_deref().map(std::path::Path::new))
        .map_err(SetupError)?;
    let uses_placeholder = cli
        .header
        .iter()
        .any(|item| item.contains(SESSION_PLACEHOLDER));
    let headers = parse_headers(&cli.header, &session_id).map_err(SetupError)?;
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
    // The context-budget guard exists only when the host names a window (ADR 0015 C1). The
    // threshold is validated only when there is a window; a threshold alone changes nothing.
    if !(1..=100).contains(&cli.context_threshold) {
        return Err(SetupError(
            "--context-threshold must be between 1 and 100".into(),
        ));
    }
    let context_budget = match cli.context_window {
        Some(0) => {
            return Err(SetupError(
                "--context-window must be at least 1 token".into(),
            ));
        }
        Some(window) => {
            Some(ContextBudget::new(window).with_threshold_percent(cli.context_threshold))
        }
        None => None,
    };
    Ok(Setup {
        model,
        servers,
        system_prompt,
        max_model_calls: cli.max_model_calls,
        turn_timeout_secs: cli.turn_timeout_secs,
        context_budget,
        session_id,
        workspace,
        secrets,
    })
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
/// workspace it was created in. The rule lives in `radian-host` so both hosts share it.
fn session_workspace(cli: &Cli) -> Result<String, SetupError> {
    Ok(
        resolve_workspace(cli.workspace.as_deref().map(std::path::Path::new))
            .map_err(SetupError)?
            .to_string_lossy()
            .into_owned(),
    )
}

/// Opens the workspace's session index and rebuilds it from the JSONL files. The index is a
/// disposable cache (ADR 0006 §10): the `.jsonl` files are the authority, and nothing here
/// writes one. `None` means the session directory does not exist yet.
fn open_workspace_index(cli: &Cli) -> Result<Option<(PathBuf, Index, String)>, SetupError> {
    let dir = session_dir(cli)?;
    let workspace = session_workspace(cli)?;
    if !dir.exists() {
        return Ok(None);
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
    Ok(Some((dir, index, workspace)))
}

/// `--list`: the sessions of this workspace, newest activity first.
fn list_sessions(cli: &Cli) -> Result<(), SetupError> {
    let Some((_dir, index, workspace)) = open_workspace_index(cli)? else {
        return Ok(()); // no sessions yet
    };
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

/// The maximum number of `--search` hits printed in one run.
const SEARCH_LIMIT: usize = 200;

/// `--export`: one session of the current workspace, Markdown or JSON. It reads the session
/// file directly (no index), so a read-only command writes nothing into the session
/// directory.
fn export_session(cli: &Cli, session_id: &str) -> Result<(), SetupError> {
    // The id becomes a file name; validate it like every other entry point so `../x` or `a/b`
    // cannot name a file outside the session directory. Without this, the workspace check
    // below is the only guard, and a forged file that claims the current workspace passes it.
    validate_session_id(session_id).map_err(SetupError)?;
    let dir = session_dir(cli)?;
    let workspace = session_workspace(cli)?;
    let path = dir.join(format!("{session_id}.jsonl"));
    if !path.exists() {
        return Err(SetupError(format!(
            "no session {session_id:?} in {}",
            dir.display()
        )));
    }
    let session = Session::open(&path)
        .map_err(|e| SetupError(format!("cannot read session {session_id:?}: {e}")))?;
    // The same workspace filter as `--list`: a session belongs to the workspace it was
    // created in, and another workspace's id is refused by name rather than exported.
    if session_workspace_of(&session).as_deref() != Some(workspace.as_str()) {
        return Err(SetupError(format!(
            "session {session_id:?} is not in this workspace; --export only exports the \
             current workspace"
        )));
    }
    let mut opts = ExportOptions::new()
        .include_reasoning(cli.include_reasoning)
        .include_provider_payload(cli.include_provider_payload);
    for secret in export_secrets(cli) {
        opts = opts.with_secret(secret);
    }
    let rendered = match cli.format {
        ExportFormat::Markdown => export_markdown(&session, &opts),
        ExportFormat::Json => export_json(&session, &opts),
    }
    .map_err(|e| SetupError(format!("cannot export session {session_id:?}: {e}")))?;
    match &cli.out {
        Some(path) => {
            if path.exists() && !cli.force {
                return Err(SetupError(format!(
                    "{} already exists; pass --force to overwrite it",
                    path.display()
                )));
            }
            std::fs::write(path, rendered)
                .map_err(|e| SetupError(format!("cannot write {}: {e}", path.display())))?;
        }
        None => print!("{rendered}"),
    }
    Ok(())
}

/// The workspace recorded in a session's `SessionCreated` event. `None` for a file the reader
/// accepted but that has no `SessionCreated` (impossible for one this CLI wrote).
fn session_workspace_of(session: &Session) -> Option<String> {
    session.events().iter().find_map(|event| match &event.data {
        EventData::Other(data) => data
            .get("workspace")
            .and_then(|workspace| workspace.as_str())
            .map(str::to_owned),
        _ => None,
    })
}

/// The values that must never appear in an export: this process's key (the one `setup` would
/// read) and every `--header` value. Redaction itself is not optional (ADR 0013 decision 3).
fn export_secrets(cli: &Cli) -> Vec<String> {
    let mut secrets = Vec::new();
    if let Ok(key) = std::env::var(&cli.api_key_env)
        && !key.is_empty()
    {
        secrets.push(key);
    }
    for header in &cli.header {
        if let Some((_, value)) = header.split_once('=')
            && !value.is_empty()
        {
            secrets.push(value.to_owned());
        }
    }
    secrets
}

/// `--search`: substring search over the current workspace's message text, using the same
/// `Index::search` the `--list` cache is built on. The index lives beside the sessions in
/// `index.db`; it is a disposable cache, never a session file (ADR 0006 §10).
fn search_sessions(cli: &Cli, query: &str) -> Result<(), SetupError> {
    let Some((_dir, index, workspace)) = open_workspace_index(cli)? else {
        eprintln!("no match");
        return Ok(());
    };
    let hits = index
        .search(query, Some(&workspace), SEARCH_LIMIT)
        .map_err(|e| SetupError(format!("cannot search: {e}")))?;
    if hits.is_empty() {
        eprintln!("no match");
        return Ok(());
    }
    // The hit's own timestamp is not part of `SearchHit`; the session's last activity is the
    // same value `--list` shows and is enough to tell hits apart.
    let times: HashMap<String, String> = index
        .list(Some(&workspace))
        .map_err(|e| SetupError(format!("cannot list sessions: {e}")))?
        .into_iter()
        .map(|session| {
            (
                session.session_id,
                session.last_ts.unwrap_or_else(|| "-".into()),
            )
        })
        .collect();
    for hit in hits {
        let time = times
            .get(&hit.session_id)
            .cloned()
            .unwrap_or_else(|| "-".into());
        println!("{}\t{}\t{}", hit.session_id, time, one_line(&hit.snippet));
    }
    Ok(())
}

/// A hit snippet as one terminal line: every run of whitespace (including the newlines a
/// multi-line message carries) becomes a single space.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
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
        serde_json::json!({"provider": "openai-compatible", "model": model_name(cli)?}),
    );
    let journal = SessionJournal::create(&path, session_id, created)
        .map_err(|e| SetupError(format!("cannot create session {session_id:?}: {e}")))?;
    Ok(Arc::new(journal))
}

async fn chat(setup: Setup, cli: Cli) -> Result<i32, SetupError> {
    let interactive = cli.prompt.is_none();
    let lines = LineHub::spawn();
    // One-shot mode with no terminal on stdin has nobody to ask; interactive mode reads its
    // answers from the same line hub as the prompt loop.
    let can_ask = interactive || std::io::stdin().is_terminal();
    let agent_config = AgentConfig {
        system_prompt: setup.system_prompt,
        context_budget: setup.context_budget,
        ..AgentConfig::default()
    };

    // The profile and the sandbox: what may run, and what the OS enforces. `full-access` is
    // only reachable because the user named it; a failed probe is an error, never a fallback.
    // All of this runs **before** the session file is created (dogfooding D2).
    let profile_arg = cli.profile.unwrap_or_else(profile::default_profile);
    let workspace_path = setup.workspace.clone();
    let permission_profile =
        profile::build_profile(profile_arg, &workspace_path, &cli.allow_shell_command)?;
    let sandbox = profile::build_sandbox(profile_arg, &workspace_path)?;
    let (profile_line, sandbox_facts) = profile::describe(profile_arg, sandbox.as_ref());
    eprintln!("[profile] {profile_line}");
    eprintln!("[sandbox] {sandbox_facts}");
    match &setup.context_budget {
        Some(budget) => eprintln!(
            "[budget] context guard on: window {} tokens, threshold {}% ({} tokens). A \
             provider-reported input is trusted as reported; only the growth since that report \
             is estimated ({} ASCII bytes per token, at least 1 token per non-ASCII character)",
            budget.window_tokens,
            budget.threshold_percent,
            budget.threshold_tokens(),
            budget.bytes_per_token
        ),
        None => eprintln!(
            "[budget] context guard off: no --context-window was given, and no default window \
             is guessed (ADR 0015)"
        ),
    }

    // The built-in tools exist only with `--workspace`. `shell` is present only when a
    // sandbox exists, which is exactly `workspace-write`/`full-access` with a workspace.
    let coding = if cli.workspace.is_some() {
        let tools = match sandbox {
            Some(sandbox) => CodingTools::with_sandbox(&workspace_path, sandbox).map_err(|e| {
                SetupError(format!("--workspace {}: {e}", workspace_path.display()))
            })?,
            None => CodingTools::new(&workspace_path).map_err(|e| {
                SetupError(format!("--workspace {}: {e}", workspace_path.display()))
            })?,
        };
        Some(tools)
    } else {
        None
    };

    // Every startup check that can fail is done. Only now is the session file created; a
    // failure above leaves the session directory untouched (dogfooding D2).
    let journal = open_journal(&cli, &setup.session_id, &workspace_path.to_string_lossy())?;
    // The permission layer replaces the CLI's own approval hook: rules, "allow for this
    // session" grants, and the audit record. `--yes` swaps the approver for one that answers
    // "allow once" without asking; rules and the sandbox are untouched by it.
    let approver: Arc<dyn Approver> = if cli.yes {
        Arc::new(AutoApprover)
    } else {
        Arc::new(TerminalApprover {
            lines: lines.clone(),
            can_ask,
        })
    };
    let (audit_tx, mut audit_rx) = mpsc::unbounded_channel();
    let audit_journal = Arc::clone(&journal);
    let audit_task = tokio::spawn(async move {
        while let Some(event) = audit_rx.recv().await {
            let _ = audit_journal.append_runtime_event(event).await;
        }
    });
    let audit = JournalAudit::new(audit_tx.clone());
    let hooks = Arc::new(PermissionHooks::new(
        permission_profile,
        Some(approver),
        audit,
    ));
    let agent = Arc::new(
        Agent::new(
            setup.model,
            CliTools::new(coding, McpToolProvider::new(setup.servers)),
        )
        .with_config(agent_config)
        .with_hooks(Arc::clone(&hooks)),
    );

    let inbox = Inbox::durable(INBOX_CAPACITY, Arc::clone(&journal));
    let ui = Arc::new(Ui::default());
    let signals = spawn_signals();
    // The approval prompt and the prompt loop share stdin. While approval may read, the
    // loop must not compete for a line; with `--yes` (or nobody to ask) it may.
    ui.set_approval_may_read(can_ask && !cli.yes);
    let options = TurnOptions {
        max_model_calls: setup.max_model_calls,
        deadline: None,
        ..TurnOptions::default()
    };
    let session_path = journal.path().to_path_buf();
    let session_id = setup.session_id.clone();
    let mut driver = Driver::open(
        &agent,
        journal,
        Arc::clone(&inbox),
        Arc::clone(&ui) as Arc<dyn EventSink>,
        options,
    )
    .await
    .map_err(|e| {
        // The MCP server **processes** start here. The session file already exists and its
        // durable content is real, so it is kept; the error says how to continue it.
        SetupError(format!(
            "cannot open the session: {e}\n  the session file {} exists; after fixing the \
             problem, continue it with `--resume {session_id}`",
            session_path.display()
        ))
    })?;
    // A per-turn timeout: the driver computes a fresh deadline at each turn's start.
    driver.set_turn_timeout(setup.turn_timeout_secs.map(Duration::from_secs));
    // The handle the interactive loop uses to say "continue once" (ADR 0015 C5).
    let driver_relax = driver.budget_relax();
    let driver_ended = Arc::new(Notify::new());
    let ended = Arc::clone(&driver_ended);
    let task = tokio::spawn(async move {
        let result = driver.run().await;
        ended.notify_one();
        result
    });

    let mut run = Run {
        inbox,
        hooks: Arc::clone(&hooks),
        relax: driver_relax,
        ui: Arc::clone(&ui),
        driver_ended,
        signals,
        task,
        secrets: setup.secrets.clone(),
    };
    let code = if let Some(prompt) = cli.prompt {
        one_shot(&mut run, prompt).await
    } else {
        interactive_loop(&mut run, &lines).await
    };
    ui.end_of_turn();
    // Flush the audit: dropping every holder of the hooks (the agent, the loop context and
    // this function) and the last sender closes the channel, and the task writes what is
    // left before it ends. Nothing may exit before this.
    drop(run);
    drop(agent);
    drop(hooks);
    drop(audit_tx);
    let _ = audit_task.await;
    Ok(code)
}

/// The single place a turn's source is decided.
///
/// Both the permission layer's current-turn source and the inbox enqueue use the same value,
/// so they cannot disagree. `reset` is true for the input that starts a turn: it resets the
/// effective source, and then every accepted input reports itself. The effective source only
/// ever **lowers** (ADR 0012 decision 5), so a mid-turn non-human input takes the turn's
/// rights down and a later human input cannot raise them back. All CLI inputs are terminal
/// input today, so this never lowers yet — but the path is wired, and this is the only place
/// to change.
fn set_turn_source(hooks: &PermissionHooks, source: InputSource, reset: bool) {
    if reset {
        hooks.set_turn_source(Some(source));
    }
    hooks.observe_input_source(source);
}

/// The running pieces the two input loops need, bundled so their signatures stay small.
struct Run {
    inbox: Arc<Inbox>,
    hooks: Arc<PermissionHooks>,
    /// The one-shot relax the interactive "continue" uses (ADR 0015 C5).
    relax: BudgetRelax,
    ui: Arc<Ui>,
    driver_ended: Arc<Notify>,
    signals: mpsc::UnboundedReceiver<()>,
    task: JoinHandle<Result<(), DriverError>>,
    secrets: Vec<String>,
}

/// Enqueues one terminal input and sets the turn source to match. This is the only call site
/// of [`set_turn_source`].
// Carrying the rejected message is the runtime's contract (ADR 0011 decision 1); the
// error is large because the message is, exactly as in `agent_core::SteerError`.
#[allow(clippy::result_large_err)]
async fn enqueue_input(
    inbox: &Arc<Inbox>,
    hooks: &PermissionHooks,
    text: String,
    source: InputSource,
    turn_in_flight: bool,
) -> Result<agent_core::MessageId, InboxError> {
    // The input that starts a turn resets the effective source; a mid-turn one only lowers.
    set_turn_source(hooks, source, !turn_in_flight);
    if turn_in_flight {
        inbox.steer(text, source).await
    } else {
        inbox.followup(text, source).await
    }
}

/// `-p`: send one input, wait for its turn, then shut the driver down.
async fn one_shot(run: &mut Run, prompt: String) -> i32 {
    if let Err(error) = enqueue_input(&run.inbox, &run.hooks, prompt, TERMINAL_SOURCE, false).await
    {
        eprintln!("agent: {}", redact(&error.to_string(), &run.secrets));
        let _ = shutdown(&run.inbox, &run.ui, &mut run.task).await;
        return 1;
    }
    let mut cancelling = false;
    match wait_for_turn(
        &run.ui,
        &run.inbox,
        &run.driver_ended,
        &mut run.signals,
        &mut cancelling,
    )
    .await
    {
        TurnEnd::Forced => return force_exit(&mut run.task).await,
        TurnEnd::DriverEnded => return report_task(&mut run.task, &run.secrets).await,
        TurnEnd::Done => {}
    }
    if let Some(code) = report_error(
        &shutdown(&run.inbox, &run.ui, &mut run.task).await,
        &run.secrets,
    ) {
        return code;
    }
    match run.ui.last_status() {
        Some(RunStatus::Completed) => 0,
        Some(RunStatus::ContextBudget(report)) => {
            for line in budget_stop_lines(&report) {
                eprintln!("{line}");
            }
            eprintln!(
                "[budget] one-shot mode does not continue automatically; nothing was sent \
                 (exit {CONTEXT_BUDGET_EXIT})"
            );
            CONTEXT_BUDGET_EXIT
        }
        Some(status) => {
            eprintln!("agent: the turn ended as {status:?}");
            1
        }
        None => 1,
    }
}

/// The context-budget stop, worded so its source is never hidden (ADR 0015 C1): a reported
/// number says "reported", a partly estimated number shows the reported baseline and the
/// estimated growth separately, and a pure estimate is marked `~` and "estimated". An
/// estimate is never displayed as a precise report.
fn budget_stop_lines(report: &BudgetReport) -> Vec<String> {
    let usage = match report.source {
        UsageSource::Reported => format!("context {} (reported)", report.projected_tokens),
        UsageSource::ReportedPlusEstimate => format!(
            "context {} (reported {} + estimated {})",
            report.projected_tokens,
            report.baseline_input_tokens.unwrap_or_default(),
            report.estimated_growth_tokens.unwrap_or_default()
        ),
        UsageSource::Estimated => format!("context ~{} (estimated)", report.projected_tokens),
    };
    let threshold = report
        .window_tokens
        .saturating_mul(u64::from(report.threshold_percent))
        / 100;
    vec![
        format!("[budget] the model request was not sent: {usage}"),
        format!(
            "[budget] window {} tokens, threshold {}% ({} tokens), request {} bytes",
            report.window_tokens, report.threshold_percent, threshold, report.request_bytes
        ),
    ]
}

/// What the user chose after a budget stop.
enum BudgetChoice {
    Continue,
    Stop,
}

/// Asks whether to continue. `fork` is deliberately not offered as an option: this slice does
/// not implement it, and an option that does nothing would lie (it is named only as
/// "not implemented").
async fn ask_budget_continue(
    lines: &LineHub,
    signals: &mut mpsc::UnboundedReceiver<()>,
    report: &BudgetReport,
) -> BudgetChoice {
    for line in budget_stop_lines(report) {
        eprintln!("{line}");
    }
    eprint!(
        "[budget] [c] continue (the next request is let through once), [n] stop \
         (fork: not implemented) "
    );
    let _ = std::io::stderr().flush();
    tokio::select! {
        line = lines.next() => match line.as_deref() {
            Some("c") | Some("C") | Some("continue") => BudgetChoice::Continue,
            _ => BudgetChoice::Stop,
        },
        // A Ctrl-C at this prompt gives up rather than hanging.
        _ = signals.recv() => BudgetChoice::Stop,
    }
}

/// Interactive: read lines and route them into the inbox.
async fn interactive_loop(run: &mut Run, lines: &LineHub) -> i32 {
    // True once the first Ctrl-C of a wind-down has been seen: the second one forces exit.
    let mut cancelling = false;
    loop {
        if !run.ui.busy() {
            cancelling = false;
            eprint!("> ");
            let _ = std::io::stderr().flush();
        }
        match next_event(
            lines,
            &run.ui,
            &run.inbox,
            &run.driver_ended,
            &mut run.signals,
            &mut cancelling,
        )
        .await
        {
            Next::Leave => break,
            Next::Forced => return force_exit(&mut run.task).await,
            Next::DriverEnded => return report_task(&mut run.task, &run.secrets).await,
            Next::Again => {
                if let Some(RunStatus::ContextBudget(report)) = run.ui.last_status()
                    && matches!(
                        ask_budget_continue(lines, &mut run.signals, &report).await,
                        BudgetChoice::Continue
                    )
                {
                    // The user chose to continue: the next turn's first request is let
                    // through once, and the guard is back from the request after it
                    // (ADR 0015 C5). It is never a permanent switch.
                    run.relax.relax_once();
                    if let Err(error) = enqueue_input(
                        &run.inbox,
                        &run.hooks,
                        CONTINUE_MESSAGE.to_owned(),
                        TERMINAL_SOURCE,
                        false,
                    )
                    .await
                    {
                        eprintln!("agent: {}", redact(&error.to_string(), &run.secrets));
                    }
                }
                continue;
            }
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
                let result = enqueue_input(
                    &run.inbox,
                    &run.hooks,
                    line.to_owned(),
                    TERMINAL_SOURCE,
                    run.ui.busy(),
                )
                .await;
                if let Err(error) = result {
                    match &error {
                        InboxError::Full { .. } => eprintln!(
                            "agent: the inbox is full; that input was not queued (try again)"
                        ),
                        other => eprintln!("agent: {}", redact(&other.to_string(), &run.secrets)),
                    }
                    continue;
                }
                if run.ui.approval_may_read() {
                    // Do not read again until the turn has started: an approval answer the
                    // user types now must reach the approval prompt, not become the next
                    // input.
                    tokio::select! {
                        _ = run.ui.started.notified() => {}
                        _ = run.ui.finished.notified() => {}
                    }
                }
            }
        }
    }
    if let Some(code) = report_error(
        &shutdown(&run.inbox, &run.ui, &mut run.task).await,
        &run.secrets,
    ) {
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
        // Not guarded by `busy`: `RunFinished` clears `busy` before it notifies, so a guard
        // here would disable the branch exactly when the turn end arrives and the loop would
        // never see it (a lost wakeup). A stale permit costs one extra loop turn, nothing more.
        _ = ui.finished.notified() => Next::Again,
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
