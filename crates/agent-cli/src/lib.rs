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
mod chat;
mod command;
mod error;
mod events;
mod inspect;
mod jsonl;
/// `radian login` / `radian logout` (ADR 0026 D3).
mod login;
mod profile;
mod redact;
mod setup;
mod tools;
mod trust;
mod turn;
mod ui;

pub use command::{Command, ConfigCommand};
pub(crate) use command::{ExportFormat, run_config, run_memory};
pub(crate) use error::{OutputError, SetupError, finish};
pub(crate) use redact::redact;
pub(crate) use setup::Setup;

use std::path::PathBuf;

use clap::Parser;
use radian_host::memory::commands::MemoryCommand;

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
    /// The model name sent to the API — a bare name, never `<provider>/<model>`; choose the
    /// provider with `--provider`. Required to run a conversation (`-p` or interactive)
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

    /// Expose the built-in file tools rooted at this directory (read/write/patch/list,
    /// glob/grep, git_diff). The `shell` tool is exposed as well: `workspace` confines it,
    /// `full` (the default) is unsandboxed.
    #[arg(long)]
    workspace: Option<String>,

    /// Permission profile: `workspace` (sandboxed: writes confined to the workspace,
    /// `shell` sandboxed, network on), or `full` (the default since ADR 0043: explicitly
    /// unsandboxed). A project's `.radian/config.toml` can narrow the result but never widen it.
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

    /// One-shot mode: run this prompt, then exit. `-p -` reads the prompt from stdin to
    /// EOF; a literal `-` prompt is not possible.
    #[arg(short, long, conflicts_with = "list")]
    prompt: Option<String>,

    /// Machine-readable output: `--output jsonl` prints every durable journal line, the
    /// live-only delta lines and a closing `Result` record to stdout, one JSON object per
    /// line. Omitted, stdout carries only the model text.
    #[arg(long, value_name = "FORMAT", value_enum)]
    output: Option<jsonl::OutputMode>,

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

    /// The per-turn model call limit. The default is shared with the TUI and named in
    /// `--help`; agent-core's own default is a lower fuse for a host that sets no limit.
    #[arg(long, value_name = "N", default_value_t = radian_host::DEFAULT_MAX_MODEL_CALLS)]
    max_model_calls: u32,

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
    // ADR 0043 decision 1a (fix round 1 A): a subagent child receives its parent's `--header`
    // values in `RADIAN_CHILD_HEADERS`. They are read — and the variable is removed from this
    // process's environment — **here**, before the runtime below is built: `remove_var` may only
    // run while the process has one thread, and the multi-thread runtime is what starts the
    // others. From here on the headers live only in the `Setup` this run builds, so no process
    // this child starts can inherit the secret.
    let child_headers = radian_host::take_child_headers_from_env();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let code = runtime.block_on(run(cli, child_headers));
    // The session is closed on every path inside `run`. Dropping a runtime waits for its
    // blocking threads, and the stdin reader's blocking read cannot be interrupted: with stdin
    // still open it would hold the process here until the user typed another line.
    runtime.shutdown_background();
    code
}

pub async fn run(mut cli: Cli, child_headers: Result<Vec<String>, String>) -> i32 {
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
        return finish(inspect::list_sessions(&cli));
    }
    // The session-management commands never touch the model or the network: they run before
    // `setup` (so no key is required) and never create a session file.
    if let Some(session_id) = &cli.export {
        return finish(inspect::export_session(&cli, session_id));
    }
    if let Some(query) = &cli.search {
        return finish(inspect::search_sessions(&cli, query));
    }
    if let Some(parent_id) = &cli.fork {
        return match inspect::fork_session(&cli, parent_id).await {
            Ok(()) => 0,
            Err((message, code)) => {
                eprintln!("radian: {message}");
                code
            }
        };
    }
    let setup = match setup::setup(&cli, child_headers).await {
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
    match chat::chat(setup, cli).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("radian: {}", redact(&e.0, &redact_secrets));
            USAGE_ERROR
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// decision 2026-10-17: `radian exec`'s per-turn model call limit defaults to the
    /// value both hosts share (1000), **not** agent-core's own `DEFAULT_MAX_MODEL_CALLS` (32).
    /// Mutation: point the field's `default_value_t` at `agent_core::DEFAULT_MAX_MODEL_CALLS`
    /// → this fails.
    #[test]
    fn the_default_model_call_limit_is_the_shared_host_value() {
        let cli = Cli::try_parse_from(["radian exec", "--model", "m"]).unwrap();
        assert_eq!(cli.max_model_calls, radian_host::DEFAULT_MAX_MODEL_CALLS);
        assert_eq!(radian_host::DEFAULT_MAX_MODEL_CALLS, 1000);
        assert_ne!(
            radian_host::DEFAULT_MAX_MODEL_CALLS,
            agent_core::DEFAULT_MAX_MODEL_CALLS,
            "the host default is deliberately higher than the runtime's own fuse"
        );
        let explicit =
            Cli::try_parse_from(["radian exec", "--model", "m", "--max-model-calls", "5"]).unwrap();
        assert_eq!(explicit.max_model_calls, 5);
    }
}
