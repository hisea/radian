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

use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::io::AsyncBufReadExt;
use tokio::sync::Notify;

use agent_core::{AgentEvent, ContextBudget, EventSink, InputSource, RunStatus, TurnOptions};
use agent_extensions::McpServerConfig;
use clap::Parser;
use radian_host::memory::MemoryService;
use radian_host::memory::commands::{self, CommandError, MemoryCommand};
use radian_host::{
    DEFAULT_MAX_TOKENS, HostModel, ModelArgs, Provider, SESSION_PLACEHOLDER, Thinking, build_model,
    check_provider_flags, generate_session_id, load_mcp_config, parse_headers, resolve_workspace,
    validate_session_id,
};
use radian_permissions::{Approver, AuditSink, PermissionHooks};
use radian_runtime::{CancelCause, Inbox, InboxError};
use radian_session::{EventData, Index, Session};
use tokio::sync::{Mutex, mpsc};

use crate::approval::{AutoApprover, JournalAudit, TERMINAL_SOURCE, TerminalApprover};

/// Exit code for usage and configuration errors, matching clap's own usage failures.
const USAGE_ERROR: i32 = 2;

/// How many inputs may wait in the inbox. Bounded on purpose (ADR 0011 L4): a full queue
/// refuses and hands the message back instead of dropping it.

#[derive(Parser)]
#[command(
    name = "agent",
    version,
    about = "Chat with a model, with optional MCP tools",
    // A subcommand (`agent memory …`) is a mode of its own, like `--list`: it must not need
    // `--model`. `--model` is not clap-required at all now (the config file may supply it),
    // so this is a no-op today; it stays so a future required argument cannot leak into the
    // subcommand modes.
    subcommand_negates_reqs = true
)]
struct Cli {
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

    /// Expose the built-in file and read-only git tools rooted at this directory
    /// (read/write/patch/list, git_status/git_diff). Under `workspace` this also
    /// exposes `shell`, which runs only inside the sandbox.
    #[arg(long)]
    workspace: Option<String>,

    /// Permission profile: `safe` (read-only, no shell, no MCP), `workspace` (the
    /// default: writes confined to the workspace, `shell` sandboxed, network on), or `full`
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

    /// A mode of its own: `agent memory …` (ADR 0021) or `agent config …` (ADR 0008). Neither
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

    /// Approve every tool call without asking.
    #[arg(long)]
    yes: bool,

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
}

/// Why startup failed. The message must not contain the key.
pub(crate) struct SetupError(pub(crate) String);

/// Why a "print and exit" command (`--list`, `--search`, `--export`, `agent memory`) stopped.
///
/// A closed stdout (`agent --list | head`) is a normal way to stop, not a failure (dogfooding
/// D21); it is kept apart from a real error so it exits 0 with no message.
pub(crate) enum OutputError {
    /// A setup, usage or memory-command failure: the message is printed, this code returned.
    Failed { message: String, code: i32 },
    /// stdout's reader went away: stop silently with exit code 0.
    BrokenPipe,
    /// Another stdout write error: one stderr line, exit code 1.
    Write(std::io::Error),
}

impl From<SetupError> for OutputError {
    fn from(error: SetupError) -> Self {
        Self::Failed {
            message: error.0,
            code: USAGE_ERROR,
        }
    }
}

impl From<CommandError> for OutputError {
    fn from(error: CommandError) -> Self {
        Self::Failed {
            message: error.message,
            code: error.code,
        }
    }
}

impl From<radian_host::PiMigrateError> for OutputError {
    fn from(error: radian_host::PiMigrateError) -> Self {
        Self::Failed {
            message: error.to_string(),
            code: 1,
        }
    }
}

impl From<radian_host::StdoutError> for OutputError {
    fn from(error: radian_host::StdoutError) -> Self {
        match error {
            radian_host::StdoutError::BrokenPipe => Self::BrokenPipe,
            radian_host::StdoutError::Failed(error) => Self::Write(error),
        }
    }
}

impl OutputError {
    /// The process exit code for this failure.
    fn exit_code(&self) -> i32 {
        match self {
            OutputError::Failed { code, .. } => *code,
            OutputError::BrokenPipe => 0,
            OutputError::Write(_) => 1,
        }
    }

    /// The line to print to stderr, if any. A broken pipe prints nothing.
    fn message(&self) -> Option<String> {
        match self {
            OutputError::Failed { message, .. } => Some(format!("agent: {message}")),
            OutputError::BrokenPipe => None,
            OutputError::Write(error) => Some(format!("agent: cannot write to stdout: {error}")),
        }
    }
}

/// Prints an output command's failure (if any) and returns its exit code.
fn finish(result: Result<(), OutputError>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(error) => {
            if let Some(line) = error.message() {
                eprintln!("{line}");
            }
            error.exit_code()
        }
    }
}

/// The `--model` value. Clap requires it unless `--list`, `--export` or `--search` is present,
/// so a conversation always has one; this makes the failure explicit instead of a panic if
/// that invariant ever changes.
fn model_name<'a>(
    cli: &'a Cli,
    config: &'a radian_host::config::Config,
) -> Result<&'a str, SetupError> {
    cli.model
        .as_deref()
        .or(config.model.model.as_deref())
        .ok_or_else(|| {
            SetupError("no model: pass --model, or set model.model in the config file".into())
        })
}

/// The `--format` value for `--export`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum ExportFormat {
    Markdown,
    Json,
}

/// The top-level subcommands. `agent` also has flag-only modes (`--list`, `--export`,
/// `--search`); those stay flags for compatibility. The subcommand name comes from the
/// variant, not the field that stores it.
#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Manage durable memory (ADR 0021 D7).
    Memory {
        #[command(subcommand)]
        command: MemoryCommand,
    },
    /// Configuration tools (ADR 0008): `agent config --help`.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

/// The `agent config` grammar. `import-pi` is the only entry point in this version, and it is
/// read-only unless `--write` is given.
#[derive(clap::Subcommand, Debug)]
enum ConfigCommand {
    /// Draft a Radian configuration from a Pi agent directory: the draft goes to stdout and
    /// the mapping report to stderr. Nothing is written without `--write`.
    ImportPi {
        /// The Pi agent directory to read (for example `~/.pi/agent`). There is no default:
        /// the command never reads a directory you did not name.
        #[arg(long, value_name = "DIR")]
        from: PathBuf,
        /// Write the draft to the Radian configuration path (the `--config`/`RADIAN_CONFIG`
        /// file, or `~/.radian/config/config.toml`) instead of printing it. Refuses to
        /// overwrite an existing file.
        #[arg(long)]
        write: bool,
    },
}

fn run_memory(cli: &Cli, command: MemoryCommand) -> i32 {
    finish(memory_command(cli, command))
}

fn run_config(cli: &Cli, command: ConfigCommand) -> i32 {
    finish(config_command(cli, command))
}

/// Runs one `agent config` subcommand. Deliberately before `setup`: no model, no key, no
/// session file and no network. The Pi directory is only ever the one the user named.
fn config_command(cli: &Cli, command: ConfigCommand) -> Result<(), OutputError> {
    match command {
        ConfigCommand::ImportPi { from, write } => {
            let draft = radian_host::import_pi(&from)?;
            // The report goes to stderr, so `agent config import-pi --from d > draft.toml`
            // still shows what could not be mapped.
            for note in &draft.notes {
                eprintln!("[import-pi] {note}");
            }
            if !write {
                radian_host::write_stdout(&draft.toml)?;
                eprintln!(
                    "[import-pi] nothing was written: this is a draft (pass --write to save it)"
                );
                return Ok(());
            }
            let env = std::env::var(radian_host::RADIAN_CONFIG_ENV).ok();
            let home = radian_host::radian_home();
            let path = radian_host::resolve_config_path(
                cli.config.as_deref(),
                env.as_deref(),
                home.as_deref(),
            )
            .ok_or_else(|| OutputError::Failed {
                message: "cannot resolve the Radian configuration path: pass --config, set \
                          RADIAN_CONFIG, or set HOME"
                    .into(),
                code: 1,
            })?;
            radian_host::write_pi_draft(&path, &draft.toml)?;
            eprintln!("[import-pi] wrote {}", path.display());
            Ok(())
        }
    }
}

/// Runs one `agent memory` subcommand. Deliberately before `setup`: no model, no key, no
/// session file, no network. The grammar and the execution live in
/// `radian_host::memory::commands`, shared byte-for-byte with the TUI's `/memory`.
fn memory_command(cli: &Cli, command: MemoryCommand) -> Result<(), OutputError> {
    let workspace = resolve_workspace(cli.workspace.as_deref().map(std::path::Path::new))
        .map_err(CommandError::usage)?;
    let root = radian_host::memory_dir()
        .ok_or_else(|| CommandError::usage("no RADIAN_HOME or HOME to place memory under"))?;
    let service = MemoryService::open(&workspace, root);

    let command = resolve_input(command)?;
    let json = command.json();
    let confirmed = command.yes();
    let first = commands::execute(&service, command.clone(), confirmed, json)?;
    let output = match (&first.needs_confirmation, confirmed) {
        (Some(prompt), false) => {
            confirm_or_refuse(prompt)?;
            commands::execute(&service, command, true, json)?
        }
        _ => first,
    };
    if let Some(note) = &output.note {
        eprintln!("{note}");
    }
    radian_host::write_stdout(&output.text)?;
    Ok(())
}

/// Resolves the arguments that name external input: `add`/`edit` read `-` from stdin, and
/// `import` reads `-` or a file. The shared executor only ever sees literal text.
fn resolve_input(command: MemoryCommand) -> Result<MemoryCommand, CommandError> {
    Ok(match command {
        MemoryCommand::Add {
            global,
            kind,
            key,
            content,
        } if content == "-" => MemoryCommand::Add {
            global,
            kind,
            key,
            content: read_stdin()?,
        },
        MemoryCommand::Edit {
            global,
            revision,
            id,
            content,
        } if content == "-" => MemoryCommand::Edit {
            global,
            revision,
            id,
            content: read_stdin()?,
        },
        MemoryCommand::Import {
            global,
            project,
            source,
        } => MemoryCommand::Import {
            global,
            project,
            source: read_import(&source)?,
        },
        other => other,
    })
}

fn read_stdin() -> Result<String, CommandError> {
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .map_err(|e| CommandError::failure(format!("cannot read stdin: {e}")))?;
    Ok(text)
}

fn read_import(source: &str) -> Result<String, CommandError> {
    if source == "-" {
        read_stdin()
    } else {
        std::fs::read_to_string(source)
            .map_err(|e| CommandError::failure(format!("cannot read {source}: {e}")))
    }
}

/// The confirmation for `forget`/`purge`/`split`/`merge` (ADR 0021 D7). A terminal asks;
/// without a terminal and without `--yes` the command refuses (exit 2) and does nothing.
fn confirm_or_refuse(message: &str) -> Result<(), CommandError> {
    if !std::io::stdin().is_terminal() {
        return Err(CommandError::usage(format!(
            "{message}: refusing without a terminal; pass --yes to confirm"
        )));
    }
    eprint!("{message} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|e| CommandError::failure(format!("cannot read the answer: {e}")))?;
    if answer.trim().eq_ignore_ascii_case("y") || answer.trim().eq_ignore_ascii_case("yes") {
        Ok(())
    } else {
        Err(CommandError::failure("cancelled"))
    }
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

async fn run(mut cli: Cli) -> i32 {
    // `agent memory` and `agent config` are modes of their own: no model, no key, no session
    // file, no network. They are handled before `setup`, like the session-management commands.
    if cli.command.is_some() {
        if cli.list
            || cli.export.is_some()
            || cli.search.is_some()
            || cli.prompt.is_some()
            || cli.resume.is_some()
        {
            eprintln!("agent: a subcommand cannot be combined with a session command");
            return USAGE_ERROR;
        }
        match cli.command.take() {
            Some(Command::Memory { command }) => return run_memory(&cli, command),
            Some(Command::Config { command }) => return run_config(&cli, command),
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
                eprintln!("agent: {message}");
                code
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
            eprintln!("agent: {}", redact(&e.0, &redact_secrets));
            USAGE_ERROR
        }
    }
}

struct Setup {
    model: HostModel,
    /// The provider actually chosen, for the session's config snapshot.
    provider: Provider,
    servers: Vec<McpServerConfig>,
    system_prompt: Option<String>,
    max_model_calls: Option<u32>,
    turn_timeout_secs: Option<u64>,
    /// The input budget `B` (ADR 0018), or `None` when no window is known (the budget is
    /// then off, not given a guessed default).
    context_budget: Option<ContextBudget>,
    /// The window and where it came from (ADR 0018), for the startup line.
    window: radian_host::Window,
    /// Whether the core compacts a request over `B` before sending it (ADR 0018).
    auto_compact: bool,
    /// The session id this process uses: the file name, and the `{session_id}` header value.
    session_id: String,
    /// The validated, canonical workspace (dogfooding D1). Resolved in `setup`, before any
    /// session file exists, so a bad `--workspace` fails as a workspace error and never
    /// reaches the sandbox probe or leaves a session behind (dogfooding D2).
    workspace: PathBuf,
    /// The key and every custom header value: redacted from all error output, never printed
    /// and never written to the session file.
    secrets: Vec<String>,
    /// The resolved model name (CLI or config), for the session's config snapshot.
    model_name: String,
    /// The resolved permission profile (CLI flag, config, or the default).
    profile: profile::ProfileArg,
    /// The config file actually read, for the `[config]` startup line.
    config_path: Option<PathBuf>,
    /// Known-but-unwired and unknown config keys, named (never their values) at startup.
    config_skipped: Vec<String>,
}

async fn setup(cli: &Cli) -> Result<Setup, SetupError> {
    // ADR 0008: the config file provides defaults; every CLI flag wins. A refused file (a
    // plaintext secret, an unknown permission value, an older schema_version) fails startup
    // before any session file exists.
    let config = radian_host::load_config(cli.config.as_deref())
        .map_err(|error| SetupError(format!("config: {error}")))?;
    if config.read_only {
        eprintln!(
            "[config] schema_version {} is newer than the supported v{}; reading it read-only",
            config.schema_version.unwrap_or(0),
            radian_host::SUPPORTED_SCHEMA_VERSION
        );
    }
    let model_name = model_name(cli, &config)?.to_owned();
    let profile = match cli.profile {
        Some(profile) => profile,
        None => match config.permissions.profile.as_deref() {
            Some(name) => profile::parse_profile(name)?,
            None => profile::default_profile(),
        },
    };
    let system_prompt = match (&cli.system, &cli.system_file) {
        (Some(text), _) => Some(text.clone()),
        (None, Some(path)) => Some(
            std::fs::read_to_string(path)
                .map_err(|e| SetupError(format!("cannot read --system-file {path}: {e}")))?,
        ),
        _ => None,
    };
    // ADR 0026 D0: `--provider`/config `model.provider` names a descriptor. The generic
    // `openai-compatible` descriptor is the default (the pre-D0 behaviour); the model is
    // never defaulted.
    let provider_id = cli
        .provider
        .clone()
        .or_else(|| config.model.provider.clone())
        .unwrap_or_else(|| radian_host::GENERIC_ID.to_owned());
    let descriptor = radian_host::descriptor(&provider_id).ok_or_else(|| {
        SetupError(format!(
            "unknown provider {provider_id:?}: choose one of {}",
            radian_host::descriptors()
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })?;
    let provider = descriptor.protocol_for(&model_name).adapter();
    let base_url_override = cli
        .base_url
        .clone()
        .or_else(|| config.model.base_url.clone());
    let thinking = match cli.thinking.as_deref() {
        Some(value) => value.parse::<Thinking>().map_err(SetupError)?,
        None => Thinking::Off,
    };
    // `--thinking` and `--max-tokens` are Anthropic-only; under openai-compatible they would
    // be ignored, so this is a usage error before the session file exists (dogfooding D15).
    check_provider_flags(provider, thinking, cli.max_tokens).map_err(SetupError)?;
    // The endpoint this run would talk to, before the session id (for the local-endpoint check).
    let probe_base_url = descriptor
        .model(&model_name)
        .map(|entry| entry.base_url.to_owned())
        .or_else(|| base_url_override.clone())
        .unwrap_or_else(|| descriptor.default_base_url.to_owned());
    // ADR 0026 D2: the key comes only from `auth.json`. A missing entry is a refusal unless the
    // endpoint is local (a local server needs no key), with the JSON to add.
    let credential =
        radian_host::auth::resolve_key(&provider_id).map_err(|e| SetupError(e.to_string()))?;
    if credential.is_none() && !is_local_base_url(&probe_base_url) {
        let message = match radian_host::auth::resolve_note(&provider_id)
            .map_err(|e| SetupError(e.to_string()))?
        {
            Some(note) => note,
            None => format!(
                "no credential for `{provider_id}`: add an entry to \
                     ~/.radian/credentials/auth.json, e.g.\n  {{\"{provider_id}\": {{\"type\": \
                     \"api_key\", \"key\": \"$MY_KEY\"}}}}\n(or use `/login {provider_id}` in the next \
                     release); refusing to continue because {} is not a local address",
                radian_host::redact_base_url(&probe_base_url)
            ),
        };
        return Err(SetupError(message));
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
    // The config's headers come first, so a `--header` of the same name wins.
    let mut header_args: Vec<String> = config
        .model
        .headers
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect();
    header_args.extend(cli.header.iter().cloned());
    let uses_placeholder = header_args
        .iter()
        .any(|item| item.contains(SESSION_PLACEHOLDER));
    let headers = parse_headers(&header_args, &session_id).map_err(SetupError)?;
    if uses_placeholder && cli.session_id.is_none() && cli.resume.is_none() {
        // Only when the generated id is actually observable: it is what makes the routing
        // reusable across processes.
        eprintln!("[session] {session_id} (pass --session-id to reuse it)");
    }
    // ADR 0026 D0: the descriptor decides the adapter, base URL and headers. `resolve_route`
    // refuses `--base-url`/`--header` for a non-generic descriptor.
    let route = radian_host::resolve_route(
        &provider_id,
        &model_name,
        base_url_override.as_deref(),
        credential
            .as_ref()
            .and_then(|key| key.env.get(radian_host::RADIAN_BASE_URL))
            .map(String::as_str),
        &headers,
        &session_id,
    )
    .map_err(SetupError)?;
    // The catalog matches on the hostname of the endpoint the model actually talks to (ADR 0018).
    let catalog_base_url = route.base_url.clone();
    let model = build_model(ModelArgs {
        provider: route.adapter(),
        base_url: route.base_url.clone(),
        model: model_name.clone(),
        api_key: credential.as_ref().map(|key| key.key.clone()),
        headers: route.headers.clone(),
        include_usage: cli.include_usage,
        max_tokens: cli.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        thinking,
    })
    .map_err(SetupError)?;
    let servers = match &cli.mcp_config {
        Some(path) => load_mcp_config(path).map_err(SetupError)?,
        None => Vec::new(),
    };
    // The key and every header value are secrets: no error this program prints may contain
    // any of them, and they are never written to the session file.
    let mut secrets: Vec<String> = credential
        .as_ref()
        .map(|key| key.key.clone())
        .into_iter()
        .collect();
    secrets.extend(route.headers.iter().map(|(_, value)| value.clone()));
    secrets.retain(|secret| !secret.is_empty());
    let context_window = cli.context_window.or(config.model.context_window);
    if context_window == Some(0) {
        return Err(SetupError(
            "--context-window must be at least 1 token".into(),
        ));
    }
    // The window comes from `--context-window` or the models.dev catalog (ADR 0018). A fetch
    // failure is not a startup failure: the window is then unknown and the guard stays off.
    // This runs before the session file exists.
    // ADR 0018: `R` comes from the request's own output setting; Anthropic sends
    // `max_tokens`, OpenAI-compatible sends none.
    let request_max_output = match provider {
        Provider::Anthropic => Some(cli.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS)),
        Provider::OpenAiCompatible => None,
    };
    let window = radian_host::resolve(&radian_host::WindowQuery::production(
        context_window,
        cli.catalog_provider.clone(),
        &catalog_base_url,
        &model_name,
        request_max_output,
        cli.offline_catalog,
    ))
    .await;
    // ADR 0018: `C ≤ R` refuses an explicit `--context-window` and only warns for a catalog
    // window (the budget is then off).
    match window.startup_check() {
        radian_host::WindowCheck::Ok => {}
        radian_host::WindowCheck::Refuse(message) => return Err(SetupError(message)),
        radian_host::WindowCheck::Warn(message) => eprintln!("{message}"),
    }
    if let Some(warning) = radian_host::output_ceiling_warning(request_max_output, window.output) {
        eprintln!("{warning}");
    }
    let context_budget = window
        .budget
        .filter(|tokens| *tokens > 0)
        .map(ContextBudget::new);
    Ok(Setup {
        model,
        provider,
        servers,
        system_prompt,
        max_model_calls: cli.max_model_calls,
        turn_timeout_secs: cli.turn_timeout_secs,
        context_budget,
        window,
        // `--no-auto-compact` wins; otherwise the config may turn it off.
        auto_compact: !cli.no_auto_compact && config.compaction.auto.unwrap_or(true),
        session_id,
        workspace,
        secrets,
        model_name,
        profile,
        config_path: config.source.clone(),
        config_skipped: config.skipped.clone(),
    })
}

/// Where session files live: `--session-dir`, then `RADIAN_SESSION_DIR`, then the user's
/// data directory.
///
/// Sessions are durable records, not a cache (ADR 0013 decision 4), and since ADR 0020 they
/// live under `~/.radian/sessions` (`$RADIAN_HOME/sessions` when that is set) — the same
/// root as the config and cache, but the sandbox denies read on this subdirectory.
fn session_dir(cli: &Cli) -> Result<PathBuf, SetupError> {
    if let Some(dir) = &cli.session_dir {
        return Ok(PathBuf::from(dir));
    }
    if let Ok(dir) = std::env::var("RADIAN_SESSION_DIR")
        && !dir.is_empty()
    {
        return Ok(PathBuf::from(dir));
    }
    // ADR 0020: the default is `~/.radian/sessions` (or `$RADIAN_HOME/sessions`).
    radian_host::sessions_dir().ok_or_else(|| {
        SetupError("no RADIAN_HOME or HOME to place sessions under; pass --session-dir".into())
    })
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

/// Opens the workspace's session index through the shared host layer (ADR 0006 §10: the
/// `.jsonl` files are the authority, the index is a disposable cache). `None` means the
/// session directory does not exist yet.
fn session_index(cli: &Cli) -> Result<Option<(Index, String)>, SetupError> {
    let dir = session_dir(cli)?;
    let workspace = session_workspace(cli)?;
    let index = radian_host::sessions::open_index(&dir).map_err(SetupError)?;
    Ok(index.map(|index| (index, workspace)))
}

/// `--list`: the sessions of this workspace, newest activity first.
fn list_sessions(cli: &Cli) -> Result<(), OutputError> {
    let Some((index, workspace)) = session_index(cli)? else {
        return Ok(()); // no sessions yet
    };
    for row in radian_host::sessions::list(&index, &workspace).map_err(SetupError)? {
        radian_host::write_line(&format!(
            "{}\t{}\t{}\t{}",
            row.session_id,
            row.created_ts.as_deref().unwrap_or("-"),
            row.last_ts.as_deref().unwrap_or("-"),
            row.message_count
        ))?;
    }
    Ok(())
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

/// The values that must never appear in an export: the keys in `auth.json` and every
/// `--header` value. Redaction itself is not optional (ADR 0013 decision 3). An export does
/// not know which provider wrote the session, so every usable key in the store is redacted.
fn export_secrets(cli: &Cli) -> Vec<String> {
    let mut secrets = Vec::new();
    if let Ok(entries) = radian_host::auth::list() {
        for entry in entries {
            if let Ok(Some(key)) = radian_host::auth::resolve_key(&entry.provider_id)
                && !key.key.is_empty()
            {
                secrets.push(key.key);
            }
        }
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
        profile::build_profile(profile_arg, &workspace_path, &cli.allow_shell_command)?;
    // ADR 0020 decision 3: the sandbox denies read on the **actual** sessions directory and
    // the memory directory. Create them owner-only first so Linux's bwrap (which can only mask
    // paths that exist) can hide them.
    let sessions_for_deny = session_dir(&cli)?;
    let _ = radian_host::ensure_private_dir(&sessions_for_deny);
    // ADR 0021: the memory root is under `~/.radian/` (ADR 0020), sandbox-denied, and the
    // service is the same one `agent memory` uses. `None` only when neither `RADIAN_HOME` nor
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
    let sandbox = profile::build_sandbox(profile_arg, &workspace_path, &sandbox_deny_read)?;

    // The approver and the audit sink. `--yes` swaps the approver for one that answers "allow
    // once" without asking; rules and the sandbox are untouched by it. The audit sink is a
    // channel whose drain task is spawned once the session (and its journal) exists.
    let approver: Arc<dyn Approver> = if cli.yes {
        Arc::new(AutoApprover)
    } else {
        Arc::new(TerminalApprover {
            lines: lines.clone(),
            can_ask,
        })
    };
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
    let runtime_events = {
        let secrets = Arc::new(setup.secrets.clone());
        Arc::new(move |event: &radian_session::RuntimeEvent| {
            if let radian_session::RuntimeEvent::TurnFailed { message, .. } = event {
                let message = redact(message, &secrets);
                eprintln!("agent: {message}");
                // ADR 0018's fallback: a recognizable context overflow gets one extra line.
                if radian_host::is_context_overflow(&message) {
                    eprintln!("agent: {}", radian_host::CONTEXT_OVERFLOW_HELP);
                }
            }
        })
    };

    // ADR 0023 U5: the one place a session is established; `radian-tui` calls the same function
    // with its own bindings.
    let session = radian_host::open_session(
        radian_host::SessionSetup {
            session_id: radian_host::SessionId::new(setup.session_id.clone()),
            session_dir: sessions_for_deny.clone(),
            workspace: workspace_path.clone(),
            coding_root: cli.workspace.clone().map(std::path::PathBuf::from),
            model: setup.model,
            servers: setup.servers,
            system_prompt: setup.system_prompt.clone(),
            context_budget: setup.context_budget,
            auto_compact: setup.auto_compact,
            sandbox,
            tier: profile_arg.preset(),
            profile: permission_profile,
            memory_root,
            resume: cli.resume.is_some(),
            config_snapshot: serde_json::json!({
                "provider": setup.provider.as_str(),
                "model": setup.model_name,
            }),
            turn_options: TurnOptions {
                max_model_calls: setup.max_model_calls,
                deadline: None,
                ..TurnOptions::default()
            },
            turn_timeout: setup.turn_timeout_secs.map(Duration::from_secs),
            wording: radian_host::CLI_WORDING,
            capabilities: radian_host::SessionCapability::ALL.to_vec(),
        },
        radian_host::HostBindings {
            approvals: radian_host::ApprovalRouting::Direct(approver),
            ask_user: false,
            audit,
            events: Arc::clone(&ui) as Arc<dyn EventSink>,
            projection,
            runtime_events,
            driver_stopped: {
                let ended = Arc::clone(&driver_ended);
                Arc::new(move || ended.notify_one())
            },
            before_driver: None,
            memory_warning: Arc::new(|message: &str| eprintln!("[memory] {message}")),
        },
    )
    .await
    .map_err(|error| SetupError(error.to_string()))?;
    let session = Arc::new(session);

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
    // ADR 0008: which file was read and what was ignored (never a value).
    match &setup.config_path {
        Some(path) => eprintln!("[config] {}", path.display()),
        None => eprintln!("[config] none"),
    }
    for key in &setup.config_skipped {
        eprintln!("[config] ignored key `{key}`");
    }
    // The numbers, the effective input budget B, and where they came from (ADR 0018).
    let compaction = if setup.auto_compact {
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
        eprintln!("agent: {}", redact(&error.to_string(), &run.secrets));
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
            eprintln!("agent: the turn ended as {status:?}");
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
    eprintln!("agent: cancelling (Ctrl-C again to quit)");
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
            eprintln!("agent: {}", redact(&error, secrets));
            1
        }
        radian_host::DriverEnd::Panicked(message) => {
            eprintln!("agent: the driver task ended unexpectedly: {message}");
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
                ..
            } => {
                let note = if below_target {
                    " (below target; no further compaction this turn)"
                } else {
                    ""
                };
                eprintln!("[compaction] {tokens_before} tokens → {tokens_after} tokens{note}");
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
