//! The headless command behind `radian exec`, as a library (ADR 0032): a chat client wiring
//! loop to an OpenAI-compatible model and MCP stdio tools, driven by `radian-runtime` so every
//! session is durable.
//!
//! `radian exec` parses [`Cli`] and calls [`run_blocking`]
//! the same parser and dispatch, so the two entries cannot drift.
//!
//! stdout carries only what the model said (streamed); status, prompts, reasoning and
//! errors go to stderr, so `radian exec -p "..."` can be piped. The API key is read from an
//! environment variable only, and is redacted from everything this program prints — and
//! from everything it writes to disk.

mod approval;
mod command;
mod error;
/// `radian login` / `radian logout` (ADR 0026 D3).
mod login;
mod profile;
mod redact;
mod setup;
mod tools;
mod trust;

pub use command::{Command, ConfigCommand};
pub(crate) use command::{ExportFormat, run_config, run_memory};
pub(crate) use error::{OutputError, SetupError, finish};
pub(crate) use redact::{export_secrets, redact};
pub(crate) use setup::Setup;
pub(crate) use trust::trust_prompt;

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::io::AsyncBufReadExt;
use tokio::sync::Notify;

use agent_core::{AgentEvent, EventSink, InputSource, RunStatus, TurnOptions};
use clap::Parser;
use radian_host::memory::commands::MemoryCommand;
use radian_host::{SessionFacade, validate_session_id};
use radian_permissions::{Approver, AuditSink, PermissionHooks};
use radian_runtime::{CancelCause, Inbox, InboxError};
use radian_session::{EventData, Index, Session};
use tokio::sync::{Mutex, mpsc};

use crate::approval::{JournalAudit, TERMINAL_SOURCE, TerminalApprover};

/// Exit code for usage and configuration errors, matching clap's own usage failures.
const USAGE_ERROR: i32 = 2;

/// How many inputs may wait in the inbox. Bounded on purpose (ADR 0011 L4): a full queue
/// refuses and hands the message back instead of dropping it.

#[derive(Parser, Default)]
#[command(
    // The command is `radian exec` now: its help, usage and `--version` name the program the
    // user actually ran (ADR 0032 direction; the `agent` binary is gone).
    name = "radian",
    version,
    about = "Chat with a model, with optional MCP tools",
    // A subcommand (`radian memory …`) is a mode of its own, like `--list`: it must not need
    // `--model`. `--model` is not clap-required at all now (the config file may supply it),
    // so this is a no-op today; it stays so a future required argument cannot leak into the
    // subcommand modes.
    subcommand_negates_reqs = true
)]
pub struct Cli {
    /// Model name sent to the API. Required to run a conversation (`-p` or interactive)
    /// unless the config file's `model.model` provides it; the print-and-exit modes
    /// (`--list`, `--export`, `--search`, `--fork`) do not need a model and do not read the
    /// key.
    #[arg(long)]
    model: Option<String>,

    /// Provider wire protocol: `openai-compatible` (the default, unchanged) or `anthropic`.
    #[arg(long, value_name = "PROVIDER")]
    provider: Option<String>,

    /// Everything before the provider's path (`/chat/completions` for openai-compatible,
    /// `/v1/messages` for anthropic). Defaults to the provider's public endpoint.
    #[arg(long, value_name = "URL")]
    base_url: Option<String>,

    /// The configuration file (ADR 0008). Overrides `RADIAN_CONFIG` and
    /// `~/.radian/config/config.toml`. Read-only: this slice never writes it.
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// `max_tokens` for anthropic (the Messages API requires it). Ignored for
    /// openai-compatible. Defaults to 16384.
    #[arg(long, value_name = "N")]
    max_tokens: Option<u32>,

    /// Extended thinking for anthropic: `off` (default), `adaptive`, or `enabled:<budget>`.
    /// Ignored for openai-compatible.
    #[arg(long, value_name = "MODE")]
    thinking: Option<String>,

    /// System prompt text.
    #[arg(long, conflicts_with = "system_file")]
    system: Option<String>,

    /// File containing the system prompt.
    #[arg(long)]
    system_file: Option<String>,

    /// MCP server config file (JSON).
    #[arg(long)]
    mcp_config: Option<String>,

    /// Expose the built-in file and read-only tools rooted at this directory
    /// (read/write/patch/list, glob/grep, git_diff). Under `workspace` this also
    /// exposes `shell`, which runs only inside the sandbox.
    #[arg(long)]
    workspace: Option<String>,

    /// Permission profile: `safe` (read-only, no shell, no MCP), `workspace` (sandboxed:
    /// writes confined to the workspace, `shell` sandboxed, network on), or `full` (the
    /// default since ADR 0043: explicitly unsandboxed). A project's `.radian/config.toml`
    /// can narrow the result but never widen it.
    #[arg(long, value_name = "PROFILE")]
    profile: Option<profile::ProfileArg>,

    /// Allow a simple shell command prefix without asking (repeatable), e.g.
    /// `--allow-shell-command "git status"`. Only a simple command matches; a composite
    /// (`git status; rm -x`) still asks. There is no allow rule for shell by default.
    #[arg(long = "allow-shell-command", value_name = "PREFIX")]
    allow_shell_command: Vec<String>,

    /// How `ask` is resolved (ADR 0012, ADR 0026 D9): `on-ask` asks when a rule says so,
    /// `always` asks for every call a rule does not deny, `never` turns `ask` into a denial,
    /// `auto` answers every `ask` with "allow" — deny rules still deny and the sandbox
    /// still applies; only the question is removed. Omit to fall back to the config's
    /// `[permissions] approval`, then `on-ask`.
    #[arg(long, value_name = "POLICY")]
    approval: Option<String>,

    /// Approve every tool call without asking: shorthand for `--approval auto` (ADR 0026 D9).
    #[arg(long)]
    yes: bool,

    /// One-shot mode: run this prompt, then exit.
    #[arg(short, long, conflicts_with = "list")]
    prompt: Option<String>,

    /// Extra request headers, repeatable: `--header NAME=VALUE`. The value is never
    /// printed; `Authorization` is refused because the key comes only from
    /// `~/.radian/credentials/auth.json` (ADR 0026 D2).
    /// A value may contain `{session_id}`, which expands to this process's session id.
    #[arg(long = "header", value_name = "NAME=VALUE")]
    header: Vec<String>,

    /// A stable session identifier for this process: used verbatim as the session file's
    /// name, and available to `--header` values as `{session_id}`. Generated once per
    /// process when omitted.
    #[arg(
        long,
        value_name = "VALUE",
        conflicts_with_all = ["resume", "list", "search"]
    )]
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

    /// Fork a session (ADR 0022): create a new session whose initial history is the parent's
    /// effective history at a turn boundary. Needs no model and reads no key.
    #[arg(
        long,
        value_name = "SESSION_ID",
        conflicts_with_all = ["prompt", "resume", "list", "export", "search"]
    )]
    fork: Option<String>,

    /// The parent seq to fork at. Omitted, the parent's last turn boundary is used.
    #[arg(long, value_name = "SEQ", requires = "fork")]
    at: Option<u64>,

    /// A mode of its own: `radian memory …` (ADR 0021) or `radian config …` (ADR 0008).
    /// Neither
    /// needs `--model`, reads a key, creates a session file or touches the network. Memory is
    /// shared across sessions and resolved per project (a git repository, shared by its
    /// worktrees).
    #[command(subcommand)]
    command: Option<Command>,

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

    /// Allow a simple shell command prefix without asking (repeatable), e.g.
    /// `--allow-shell-command "git status"`. Only a simple command matches; a composite
    /// (`git status; rm -x`) still asks. There is no allow rule for shell by default.

    /// Override the per-turn model call limit.
    #[arg(long)]
    max_model_calls: Option<u32>,

    /// Per-turn time limit in seconds.
    #[arg(long)]
    turn_timeout_secs: Option<u64>,

    /// The model's context window in tokens. Omitted, the models.dev catalog is consulted
    /// for `(provider, model)` (ADR 0018); an explicit value always wins. With neither, the
    /// budget is off and no number is guessed (ADR 0018).
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

    /// Trust this workspace's project resources for **this process only** (never written to
    /// `trust.json`; ADR 0031 T3).
    #[arg(long, conflicts_with = "no_trust")]
    trust: bool,

    /// Do not trust this workspace's project resources for this process only (ADR 0031 T3).
    #[arg(long)]
    no_trust: bool,

    /// Hidden: the parent session of a derived session (ADR 0043 decision 1a's subagent child).
    /// Written to `SessionCreated.parent` with `relation = "subagent"`; a session with this
    /// parent never mounts `subagent` itself (depth 1).
    #[arg(long, value_name = "SESSION_ID", hide = true)]
    parent: Option<String>,

    /// Hidden: the parent seq the derived session records (0 when the spawner knows no turn
    /// boundary).
    #[arg(long, value_name = "SEQ", default_value_t = 0, hide = true)]
    parent_seq: u64,

    /// Hidden: mount only these built-in tools, comma-separated. The `explore` subagent child
    /// is started with `read,list,glob,grep`; a name not listed is not mounted at all.
    #[arg(long = "tools", value_name = "NAME,...", hide = true)]
    tools: Option<String>,
}

impl Cli {
    /// A `Cli` carrying only one management command, everything else at its default. The
    /// top-level `radian memory`/`login`/`config` build this so they run the **same** dispatch
    /// the parser feeds (ADR 0032 direction).
    pub fn for_command(command: Command) -> Self {
        Self {
            command: Some(command),
            ..Self::default()
        }
    }

    /// [`Cli::for_command`] with the workspace `radian exec --workspace DIR memory …` would
    /// carry.
    pub fn for_memory(workspace: Option<String>, command: MemoryCommand) -> Self {
        Self {
            command: Some(Command::Memory { command }),
            workspace,
            ..Self::default()
        }
    }
}

/// Parses `args` (program name included) and runs it to completion, returning the process
/// exit code. This is what `radian exec` calls (the TUI passes the argv): clap's own errors
/// (a usage error, `--help`, `--version`) exit inside [`Cli::parse_from`], exactly as the
/// binary always did.
pub fn parse_and_run<I, T>(args: I) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    run_blocking(Cli::parse_from(args))
}

/// Runs the parsed CLI to completion and returns the exit code: the runtime is built here and
/// torn down with a bound, exactly as the headless command always did.
pub fn run_blocking(cli: Cli) -> i32 {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let code = runtime.block_on(run(cli));
    // The session is closed on every path inside `run`. Dropping a runtime waits for its
    // blocking threads, and the stdin reader's blocking read cannot be interrupted: with stdin
    // still open it would hold the process here until the user typed another line.
    runtime.shutdown_background();
    code
}

pub async fn run(mut cli: Cli) -> i32 {
    // `radian memory` and `radian config` are modes of their own: no model, no key, no session
    // file, no network. They are handled before `setup`, like the session-management commands.
    if cli.command.is_some() {
        if cli.list
            || cli.export.is_some()
            || cli.search.is_some()
            || cli.prompt.is_some()
            || cli.resume.is_some()
        {
            eprintln!("radian: a subcommand cannot be combined with a session command");
            return USAGE_ERROR;
        }
        match cli.command.take() {
            Some(Command::Memory { command }) => return run_memory(&cli, command),
            Some(Command::Config { command }) => return run_config(&cli, command),
            // D3: the credential commands are modes of their own — no model, no session
            // file; `radian login` talks to the named endpoint once (the verification).
            Some(Command::Login { provider }) => return login::run_login(provider).await,
            Some(Command::Logout { provider }) => return login::run_logout(provider).await,
            None => unreachable!("checked just above"),
        }
    }
    if cli.list {
        return finish(list_sessions(&cli));
    }
    // The session-management commands never touch the model or the network: they run before
    // `setup` (so no key is required) and never create a session file.
    if let Some(session_id) = &cli.export {
        return finish(export_session(&cli, session_id));
    }
    if let Some(query) = &cli.search {
        return finish(search_sessions(&cli, query));
    }
    if let Some(parent_id) = &cli.fork {
        return match fork_session(&cli, parent_id).await {
            Ok(()) => 0,
            Err((message, code)) => {
                eprintln!("radian: {message}");
                code
            }
        };
    }
    let setup = match setup::setup(&cli).await {
        Ok(setup) => setup,
        Err(e) => {
            eprintln!("radian: {}", e.0);
            return USAGE_ERROR;
        }
    };
    // ADR 0020 decision 5: never move a user's data; print a copy-pasteable command instead.
    // Only when the default location is in use (an explicit --session-dir opted out).
    if cli.session_dir.is_none()
        && std::env::var("RADIAN_SESSION_DIR").map_or(true, |dir| dir.is_empty())
        && let Some(hint) = radian_host::legacy_sessions_hint()
    {
        eprintln!("{hint}");
    }
    let redact_secrets = setup.secrets.clone();
    match chat(setup, cli).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("radian: {}", redact(&e.0, &redact_secrets));
            USAGE_ERROR
        }
    }
}

/// [`radian_host::session_dir`] with this host's error type (ADR 0032 slice 5: the rule lives in
/// `radian-host`, so the two hosts cannot drift).
fn session_dir(cli: &Cli) -> Result<PathBuf, SetupError> {
    radian_host::session_dir(cli.session_dir.as_deref().map(std::path::Path::new))
        .map_err(SetupError)
}

/// The workspace a session belongs to: `--workspace` when given, else the current directory
/// (canonicalized and validated by the shared `radian-host` rule). `--list` filters by the same
/// value, so a session is only listed in the workspace it was created in.
fn session_workspace(cli: &Cli) -> Result<String, SetupError> {
    radian_host::session_workspace(cli.workspace.as_deref().map(std::path::Path::new))
        .map_err(SetupError)
}

/// Opens the workspace's session index through the shared host layer (ADR 0006 §10: the
/// `.jsonl` files are the authority, the index is a disposable cache). `None` means the
/// session directory does not exist yet.
fn session_index(cli: &Cli) -> Result<Option<(Index, String)>, SetupError> {
    let dir = session_dir(cli)?;
    let workspace = session_workspace(cli)?;
    let index = radian_host::sessions::open_index(&dir).map_err(SetupError)?;
    Ok(index.map(|index| (index, workspace)))
}

/// `--list`: the sessions of this workspace, newest activity first (the shared table, ADR 0032
/// slice 5).
fn list_sessions(cli: &Cli) -> Result<(), OutputError> {
    let dir = session_dir(cli)?;
    let workspace = session_workspace(cli)?;
    Ok(radian_host::list_sessions(&dir, &workspace)?)
}

/// The maximum number of `--search` hits printed in one run.
const SEARCH_LIMIT: usize = 200;

/// `--export`: one session of the current workspace, Markdown or JSON. It reads the session
/// file directly (no index), so a read-only command writes nothing into the session
/// directory.
fn export_session(cli: &Cli, session_id: &str) -> Result<(), OutputError> {
    let dir = session_dir(cli)?;
    let workspace = session_workspace(cli)?;
    let format = match cli.format {
        ExportFormat::Markdown => radian_host::Format::Markdown,
        ExportFormat::Json => radian_host::Format::Json,
    };
    let settings = radian_host::ExportSettings {
        include_reasoning: cli.include_reasoning,
        include_provider_payload: cli.include_provider_payload,
        secrets: export_secrets(cli),
    };
    // The shared host layer validates the id (`../x` cannot name a file), checks the workspace
    // (another workspace's session is refused by name) and redacts; this CLI only decides
    // where the bytes go.
    let rendered = radian_host::sessions::export(&dir, &workspace, session_id, format, &settings)
        .map_err(SetupError)?;
    match &cli.out {
        Some(path) => {
            if path.exists() && !cli.force {
                return Err(SetupError(format!(
                    "{} already exists; pass --force to overwrite it",
                    path.display()
                ))
                .into());
            }
            std::fs::write(path, rendered)
                .map_err(|e| SetupError(format!("cannot write {}: {e}", path.display())))?;
        }
        None => radian_host::write_stdout(&rendered)?,
    }
    Ok(())
}

/// `--search`: substring search over the current workspace's message text, using the same
/// `Index::search` the `--list` cache is built on. The index lives beside the sessions in
/// `index.db`; it is a disposable cache, never a session file (ADR 0006 §10).
fn search_sessions(cli: &Cli, query: &str) -> Result<(), OutputError> {
    let Some((index, workspace)) = session_index(cli)? else {
        eprintln!("no match");
        return Ok(());
    };
    let hits = radian_host::sessions::search(&index, &workspace, query, SEARCH_LIMIT)
        .map_err(SetupError)?;
    if hits.is_empty() {
        eprintln!("no match");
        return Ok(());
    }
    let secrets = export_secrets(cli);
    for hit in hits {
        // The snippet is session content and can carry a model-echoed key or header value;
        // redact the known secrets exactly like `--export` (ADR 0013 decision 3).
        let snippet = redact(&hit.snippet, &secrets);
        radian_host::write_line(&format!(
            "{}\t{}\t{}",
            hit.session_id,
            hit.last_ts.as_deref().unwrap_or("-"),
            snippet
        ))?;
    }
    Ok(())
}

/// Like the provider's own rule: localhost, 127.0.0.1 and ::1 may go without a key.
/// `--fork <parent>` (ADR 0022): create a child session from the parent's effective history
/// at a turn boundary. No model, no key, no network. A parent another process is writing exits
/// 1 rather than waiting; other failures are usage errors.
async fn fork_session(cli: &Cli, parent_id: &str) -> Result<(), (String, i32)> {
    let usage = |message: String| (message, USAGE_ERROR);
    validate_session_id(parent_id).map_err(usage)?;
    let dir = session_dir(cli).map_err(|e| (e.0, USAGE_ERROR))?;
    let parent_path = dir.join(format!("{parent_id}.jsonl"));
    if !parent_path.exists() {
        return Err(usage(format!(
            "no session {parent_id:?} in {}",
            dir.display()
        )));
    }
    let child_id = cli
        .session_id
        .clone()
        .unwrap_or_else(radian_host::generate_session_id);
    validate_session_id(&child_id).map_err(usage)?;
    let child_path = dir.join(format!("{child_id}.jsonl"));
    if child_path.exists() {
        return Err(usage(format!(
            "session {child_id:?} already exists; pass --session-id with a new id"
        )));
    }
    let parent = Session::open(&parent_path)
        .map_err(|e| usage(format!("cannot read session {parent_id:?}: {e}")))?;
    // ADR 0022 §5: the child shares the parent's workspace and inherits its config snapshot.
    let created = created_data(&parent);
    let workspace = created
        .and_then(|data| data["workspace"].as_str().map(str::to_owned))
        .ok_or_else(|| usage(format!("session {parent_id:?} has no workspace")))?;
    let config_snapshot = created
        .and_then(|data| data.get("config_snapshot").cloned())
        .unwrap_or_else(|| serde_json::json!({}));
    let request = radian_session::ForkRequest {
        parent_path,
        parent_id: parent_id.to_owned(),
        seq: cli.at,
        child_path,
        child_id: child_id.clone(),
        workspace,
        config_snapshot,
    };
    match radian_session::fork(request).await {
        Ok(outcome) => {
            println!(
                "forked {parent_id} at seq {} into {}",
                outcome.seq, outcome.child_id
            );
            println!("continue it with `--resume {}`", outcome.child_id);
            Ok(())
        }
        // The parent is being written by another process: say so and exit 1, do not wait.
        Err(radian_session::ForkError::Parent {
            source: radian_session::WriteError::Locked { .. },
            ..
        }) => Err((
            format!("session {parent_id:?} is being written by another process; stop it first"),
            1,
        )),
        Err(error) => Err(usage(error.to_string())),
    }
}

/// The parsed `SessionCreated` data of a session, so a fork can inherit the parent's
/// workspace and model-route reference.
fn created_data(session: &Session) -> Option<&serde_json::Value> {
    session.events().iter().find_map(|event| match &event.data {
        EventData::Other(data) if event.event == "SessionCreated" => Some(data),
        _ => None,
    })
}

async fn chat(setup: Setup, cli: Cli) -> Result<i32, SetupError> {
    let interactive = cli.prompt.is_none();
    let lines = LineHub::spawn();
    // One-shot mode with no terminal on stdin has nobody to ask; interactive mode reads its
    // answers from the same line hub as the prompt loop.
    let can_ask = interactive || std::io::stdin().is_terminal();

    // The profile and the sandbox: what may run, and what the OS enforces. `full` is only
    // reachable because the user named it; a failed probe is an error, never a fallback. All
    // of this runs **before** the session file is created (dogfooding D2).
    let profile_arg = setup.profile;
    let workspace_path = setup.workspace.clone();
    let permission_profile =
        profile::build_profile(profile_arg, &workspace_path, &cli.allow_shell_command)?
            .with_approval(setup.approval.policy);
    // ADR 0020 decision 3: the sandbox denies read on the **actual** sessions directory and
    // the memory directory. Create them owner-only first so Linux's bwrap (which can only mask
    // paths that exist) can hide them.
    let sessions_for_deny = session_dir(&cli)?;
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
    let ui = Arc::new(Ui::default());
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

    // ADR 0030 slice 8: the process control port. Bound before the session so its fan-out can
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
    // unwind, even while the driver task still holds a reference to the server (ADR 0030 O6).
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
        base_url: setup.base_url.clone(),
        config: cli.config.clone(),
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
    // The hidden `--tools` list is a physical unmount (the `explore` child's four read tools).
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
                max_model_calls: setup.max_model_calls,
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
            memory_notice_line: Arc::new(|line: &str| eprintln!("[memory] {line}")),
        },
    )
    .await
    .map_err(|error| SetupError(error.to_string()))?;
    let session = Arc::new(session);
    // ADR 0030 O6: start accepting control requests for this session.
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
    let recorded_access = session_dir(&cli)
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
    let mut run = Run {
        inbox: Arc::clone(session.inbox()),
        hooks: Arc::clone(session.hooks()),
        ui: Arc::clone(&ui),
        driver_ended,
        signals,
        session: Arc::clone(&session),
        secrets: setup.secrets.clone(),
    };
    let code = if let Some(prompt) = cli.prompt {
        one_shot(&mut run, prompt).await
    } else {
        interactive_loop(&mut run, &lines).await
    };
    ui.end_of_turn();
    // Flush the audit: dropping the session drops the agent and the hooks, and dropping the
    // last sender closes the channel; the task writes what is left before it ends.
    drop(run);
    drop(session);
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
    ui: Arc<Ui>,
    driver_ended: Arc<Notify>,
    signals: mpsc::UnboundedReceiver<()>,
    /// The session facade (ADR 0023 U5): its driver handle, its journal and its projection.
    session: Arc<radian_host::LocalSession>,
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
        eprintln!("radian: {}", redact(&error.to_string(), &run.secrets));
        let _ = shutdown(&run.inbox, &run.ui, &run.session).await;
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
        TurnEnd::Forced => return force_exit(&run.session).await,
        TurnEnd::DriverEnded => return report_task(&run.session, &run.secrets).await,
        TurnEnd::Done => {}
    }
    if let Some(code) = report_error(
        &shutdown(&run.inbox, &run.ui, &run.session).await,
        &run.secrets,
    ) {
        return code;
    }
    match run.ui.last_status() {
        Some(RunStatus::Completed) => 0,
        Some(status) => {
            eprintln!("radian: the turn ended as {status:?}");
            1
        }
        None => 1,
    }
}

/// Interactive: read lines and route them into the inbox.
/// Recognizes a `/compact [instructions]` line (ADR 0018). Only a whole line whose first word
/// is exactly `/compact` counts: `/compactfoo` and `see /compact` are ordinary messages.
fn compact_request(line: &str) -> Option<Option<String>> {
    let rest = line.strip_prefix("/compact")?;
    if rest.is_empty() {
        return Some(None);
    }
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let instructions = rest.trim();
    Some((!instructions.is_empty()).then(|| instructions.to_owned()))
}

/// Runs a manual `/compact` between turns (ADR 0018) and prints the result. Ctrl-C cancels the
/// in-flight summarizer; nothing is committed then.
async fn run_compact(run: &mut Run, instructions: Option<String>) {
    let mut compact = Box::pin(run.inbox.compact(instructions));
    let result = loop {
        tokio::select! {
            result = &mut compact => break result,
            Some(()) = run.signals.recv() => {
                run.inbox.cancel(CancelCause::Requested);
            }
        }
    };
    match result {
        Ok(agent_core::CompactionOutcome::Compacted {
            tokens_before,
            tokens_after,
            ..
        }) => eprintln!("[compaction] {tokens_before} tokens → {tokens_after} tokens"),
        Ok(agent_core::CompactionOutcome::NothingToCompact) => {
            eprintln!("[compaction] nothing to compact")
        }
        Err(message) => eprintln!("[compaction] failed: {message}"),
    }
}

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
            Next::Forced => return force_exit(&run.session).await,
            Next::DriverEnded => return report_task(&run.session, &run.secrets).await,
            Next::Again => continue,
            Next::Line(line) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if line == "/exit" || line == "/quit" {
                    break;
                }
                // `/compact [instructions]` is a control request, not a message (ADR 0018).
                // Only a whole line whose first word is exactly `/compact` counts; `-p` mode
                // has no such command.
                if let Some(instructions) = compact_request(line) {
                    run_compact(run, instructions).await;
                    continue;
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
                            "radian: the inbox is full; that input was not queued (try again)"
                        ),
                        other => eprintln!("radian: {}", redact(&other.to_string(), &run.secrets)),
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
        &shutdown(&run.inbox, &run.ui, &run.session).await,
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
    eprintln!("radian: cancelling (Ctrl-C again to quit)");
    Next::Again
}

/// Stops the driver gracefully: it finishes the turns already queued, closes the session
/// (so MCP servers shut down), and returns.
async fn shutdown(
    inbox: &Arc<Inbox>,
    ui: &Arc<Ui>,
    session: &Arc<radian_host::LocalSession>,
) -> Result<(), String> {
    // Let queued inputs run: closing now would abandon them.
    while ui.busy() || !inbox.is_empty().await {
        ui.finished.notified().await;
    }
    inbox.close();
    match session.join().await {
        radian_host::DriverEnd::Ok => Ok(()),
        radian_host::DriverEnd::Failed(error) => Err(error),
        radian_host::DriverEnd::Panicked(message) => {
            Err(format!("the driver task ended unexpectedly: {message}"))
        }
    }
}

/// Aborts the driver: the session is dropped, which kills the tool processes at once. Used
/// only for the forced second Ctrl-C.
async fn force_exit(session: &Arc<radian_host::LocalSession>) -> i32 {
    session.abort();
    let _ = session.join().await;
    130
}

/// Awaits the driver task once and reports its error, if any.
async fn report_task(session: &Arc<radian_host::LocalSession>, secrets: &[String]) -> i32 {
    match session.join().await {
        radian_host::DriverEnd::Ok => 0,
        radian_host::DriverEnd::Failed(error) => {
            eprintln!("radian: {}", redact(&error, secrets));
            1
        }
        radian_host::DriverEnd::Panicked(message) => {
            eprintln!("radian: the driver task ended unexpectedly: {message}");
            1
        }
    }
}

fn report_error(result: &Result<(), String>, secrets: &[String]) -> Option<i32> {
    match result {
        Ok(()) => None,
        Err(error) => {
            eprintln!("radian: {}", redact(error, secrets));
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
                eprintln!("radian: cancelling (Ctrl-C again to quit)");
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
            AgentEvent::ModelRetrying {
                attempt,
                max,
                delay,
                reason,
            } => {
                // ADR 0017: the same line the TUI shows, on stderr, before the wait.
                // The failed attempt's streamed text went to stdout and cannot be unwritten, so
                // this only closes its line before the retry notice and the next attempt.
                self.end_of_turn();
                eprintln!("retrying {attempt}/{max} in {}s: {reason}", delay.as_secs());
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
            AgentEvent::CompactionStarted {
                tokens_before,
                keep_target,
                ..
            } => {
                eprintln!(
                    "[compaction] {tokens_before} tokens over the input budget; summarizing \
                     (keep target {keep_target})"
                );
            }
            AgentEvent::CompactionFinished {
                tokens_before,
                tokens_after,
                below_target,
                deterministic,
                dropped,
                ..
            } => {
                // ADR 0018, decision 3: a deterministic fallback says so and names
                // the messages it dropped; a model summary keeps the token line.
                if deterministic {
                    eprintln!("[compaction] compacted without a summary ({dropped} dropped)");
                } else {
                    let note = if below_target {
                        " (below target; no further compaction this turn)"
                    } else {
                        ""
                    };
                    eprintln!("[compaction] {tokens_before} tokens → {tokens_after} tokens{note}");
                }
            }
            AgentEvent::CompactionFailed { reason } => {
                eprintln!("[compaction] failed: {reason}");
            }
            AgentEvent::CompactionSkipped { reason } => {
                eprintln!("[compaction] skipped: {reason}");
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
                        eprintln!("radian: cancelled ({status:?}); type another line or /exit")
                    }
                    other => eprintln!("[turn] ended as {other:?}"),
                }
                self.finished.notify_one();
            }
            _ => {}
        }
    }
}
