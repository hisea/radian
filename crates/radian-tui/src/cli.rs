//! The `radian` command: one real conversation through `radian-runtime`, rendered full
//! screen.
//!
//! This module is the library side of the `radian`/`rad` binaries: [`run_cli`] parses the
//! command line and runs it. Both `src/main.rs` and `src/rad.rs` are thin wrappers around it,
//! so no source file belongs to two build targets (the alias is the same program).
//!
//! Never run this in a shared terminal by hand without expecting it to take over: it enters
//! the alternate screen and raw mode. The tests drive it through a pty.

use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use agent_core::{AgentEvent, ContextBudget, EventSink, TurnOptions};
use clap::Parser;
use radian_host::memory::MemoryService;
use radian_host::{
    DEFAULT_MAX_TOKENS, ModelArgs, Provider, SessionFacade, Thinking, build_model,
    check_context_window_flag, check_provider_flags, is_local_base_url, load_mcp_config,
    parse_headers, validate_session_id,
};
use radian_permissions::AuditSink;
use radian_session::SessionJournal;
use radian_tui::app::{AUTO_SCROLL_TICK, App, Effect, Input as AppInput, MemoryResult, PickerData};
use radian_tui::clip::{ClipboardMode, osc52};
use radian_tui::events::{InputEvent, ProjectionSink, UiEvent};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event};
use ratatui::crossterm::terminal;

mod attach;
mod context;
mod herdr;
mod models;
mod run;
mod session;
mod tty;
mod ui;

use attach::run_attach;
use context::*;
use run::run;

use tty::*;
use ui::*;

#[derive(Parser, Debug)]
#[command(
    name = "radian",
    version,
    about = "Full-screen TUI for the radian agent runtime",
    // ADR 0032 A6: the joined surface is longer than either binary's, so the commands and
    // the flags are grouped.
    subcommand_help_heading = "Commands"
)]
struct Cli {
    // The TUI's own setup flags come first: the default mode is the full-screen TUI, and the
    // `--help` groups read in the order the owner asked for (TUI, session, model, mcp,
    // permissions).
    /// The configuration file (ADR 0008). Overrides `RADIAN_CONFIG` and
    /// `~/.radian/config/config.toml`. Read-only: this slice never writes it.
    #[arg(help_heading = "TUI", long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Expose the built-in file and read-only git tools rooted at this directory
    /// (read/write/patch/list, git_status/git_diff). No shell is provided.
    #[arg(help_heading = "TUI", long, short = 'w', value_name = "DIR")]
    workspace: Option<PathBuf>,

    #[arg(help_heading = "TUI", long)]
    system: Option<String>,

    /// Model name sent to the API. Required unless the config file's `model.model` provides
    /// it.
    #[arg(help_heading = "Model", long, short = 'm')]
    model: Option<String>,

    /// Provider wire protocol: `openai-compatible` (the default, unchanged) or `anthropic`.
    #[arg(help_heading = "Model", long, value_name = "PROVIDER")]
    provider: Option<String>,

    /// Everything before the provider's path (`/chat/completions` for openai-compatible,
    /// `/v1/messages` for anthropic). Defaults to the provider's public endpoint.
    #[arg(help_heading = "Model", long, value_name = "URL")]
    base_url: Option<String>,

    /// `max_tokens` for anthropic (the Messages API requires it). Ignored for
    /// openai-compatible. Defaults to 16384.
    #[arg(help_heading = "Model", long, value_name = "N")]
    max_tokens: Option<u32>,

    /// Extended thinking for anthropic: `off` (default), `adaptive`, or `enabled:<budget>`.
    /// Ignored for openai-compatible.
    #[arg(help_heading = "Model", long, value_name = "MODE")]
    thinking: Option<String>,

    /// Extra request header, repeatable: `--header NAME=VALUE`.
    #[arg(help_heading = "Model", long = "header", value_name = "NAME=VALUE")]
    header: Vec<String>,

    /// Where session files live. Defaults to `RADIAN_SESSION_DIR`, then `~/.radian/sessions`
    /// (`$RADIAN_HOME/sessions` when that is set, ADR 0020). The same rule as `agent-cli`.
    #[arg(help_heading = "Session", long, value_name = "DIR")]
    session_dir: Option<PathBuf>,

    /// A stable session identifier: the session file's name. Generated once per process when
    /// omitted.
    #[arg(
        help_heading = "Session",
        long,
        value_name = "VALUE",
        conflicts_with = "resume"
    )]
    session_id: Option<String>,

    /// Resume an existing session by id, rendering its history into the transcript. Without a
    /// value it shows the session picker (resuming only: no "new" entry).
    #[arg(help_heading = "Session", long, short = 'r', value_name = "SESSION_ID", num_args = 0..=1, conflicts_with = "list")]
    resume: Option<Option<String>>,

    /// List this workspace's sessions and exit.
    #[arg(help_heading = "Session", long, short = 'l')]
    list: bool,

    /// Show a full-screen picker for this workspace's sessions before opening one (Phase 4):
    /// Up/Down select, type to filter, Tab full-text search, Enter resume, n new, Esc quit.
    /// Conflicts with `--resume`/`--session-id`; it does not create a session file.
    #[arg(help_heading = "Session", long, conflicts_with_all = ["resume", "session_id"])]
    pick: bool,

    /// A JSON file listing MCP stdio servers to start (`{"servers": [...]}`), the same format
    /// `agent-cli` reads. Under `workspace` their tools ask before running and the
    /// servers themselves are **not** sandboxed (ADR 0012 option B).
    #[arg(help_heading = "MCP", long, value_name = "FILE")]
    mcp_config: Option<String>,

    /// The permission tier (ADR 0016): `safe` denies file modification, `workspace`
    /// allows writes that resolve inside the workspace, `full` also allows MCP. Defaults to
    /// `full` (ADR 0043); a project's `.radian/config.toml` may narrow it, and the config
    /// file's `permissions.profile` is tried in between.
    #[arg(help_heading = "Permissions", long, value_name = "TIER")]
    profile: Option<String>,

    /// How `ask` is resolved (ADR 0012's ApprovalPolicy): `on-ask` asks when a rule says so,
    /// `always` asks for every call a rule does not deny, `never` turns `ask` into a denial,
    /// `auto` answers every `ask` with "allow" (ADR 0026 D9). Omit to fall back to the
    /// config's `[permissions] approval`, then the session's journaled state, then `on-ask`.
    #[arg(help_heading = "Permissions", long, value_name = "POLICY")]
    approval: Option<String>,

    /// Approve every tool call without asking, like `radian exec --yes`. Not "allow everything":
    /// `deny` rules still deny and the sandbox still applies; only the question is removed.
    #[arg(help_heading = "Permissions", long, short = 'y')]
    yes: bool,

    /// The per-turn model call limit. The core's default (`DEFAULT_MAX_MODEL_CALLS`, 32) is a
    /// fuse for unattended runs; an interactive coding session reads many files per turn and
    /// a human can cancel, so the TUI defaults much higher. It is still a fuse: a loop of
    /// small calls is not stopped by the context budget (which only triggers compaction).
    #[arg(help_heading = "Model", long, value_name = "N", default_value_t = TUI_DEFAULT_MAX_MODEL_CALLS)]
    max_model_calls: u32,

    /// Allow a shell command prefix without asking, repeatable (e.g. `--allow-shell-command
    /// "git status"`). Only a *simple* command matches: any shell metacharacter keeps the
    /// call at `ask`. Write these at the subcommand level, never at a bare program name.
    #[arg(
        help_heading = "Permissions",
        long = "allow-shell-command",
        value_name = "PREFIX"
    )]
    allow_shell_command: Vec<String>,

    /// The model's context window in tokens (ADR 0015). Omitted, the models.dev catalog is
    /// consulted for `(provider, model)` (ADR 0018); an explicit value always wins. With
    /// neither, the budget is off and the status bar says so.
    #[arg(help_heading = "Model", long, value_name = "TOKENS")]
    context_window: Option<u64>,

    /// Never fetch the models.dev catalog; use only the local cache.
    #[arg(help_heading = "Model", long)]
    offline_catalog: bool,

    /// The models.dev provider id to look the model up under (default: derived from the
    /// `--base-url` hostname; e.g. `opencode.ai` → `opencode`).
    #[arg(help_heading = "Model", long, value_name = "PROVIDER")]
    catalog_provider: Option<String>,

    /// Turn automatic compaction off. Over the input budget `B` the request is then sent
    /// unchanged (ADR 0018).
    #[arg(help_heading = "Model", long)]
    no_auto_compact: bool,

    /// Trust this workspace's project resources for **this process only** (never written to
    /// `trust.json`; ADR 0031 T3).
    #[arg(help_heading = "Permissions", long, conflicts_with = "no_trust")]
    trust: bool,

    /// Do not trust this workspace's project resources for this process only (ADR 0031 T3).
    #[arg(help_heading = "Permissions", long)]
    no_trust: bool,

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

    /// Test hook: start this command as a background job (its own process group), wait for it
    /// to write `job.pid` in the workspace, then panic. The panic path must kill the group
    /// (ADR 0037 decision 6: `Jobs::drop` -> `kill_all`, killpg only).
    #[arg(long, hide = true, value_name = "COMMAND")]
    panic_with_background_job: Option<String>,

    /// Test hook: make the driver task panic instead of running, so the UI's watch on it can
    /// be exercised.
    #[arg(long, hide = true)]
    panic_in_driver: bool,

    /// Test hook: block the event read forever (writing this marker file first), standing in
    /// for crossterm's reader spinning on a terminal that has gone away. The pty test waits for
    /// the marker and then signals, so the main loop is provably inside the read.
    #[arg(long, hide = true, value_name = "FILE")]
    block_input: Option<PathBuf>,

    /// Test hook: after the first frame, write this marker and stop drawing. With no further
    /// writes, closing the pty cannot be noticed by a failed draw, so the input poll is the
    /// only way out -- which is what `pty_a_closed_master_ends_the_app_cleanly` must exercise.
    #[arg(long, hide = true, value_name = "FILE")]
    freeze_draw: Option<PathBuf>,

    /// Test hook (`--slow-draw <MILLIS>`): `draw_frame` sleeps this long per frame, so the main
    /// loop can be made to miss the signal task's 3 s watchdog (the reader runs on its own
    /// thread since ADR 0023 U2, so `--block-input` no longer parks the loop).
    #[arg(long, hide = true, value_name = "MILLIS")]
    slow_draw: Option<u64>,

    /// Test hook (`--flood-facts <COUNT>`): send this many facts as fast as possible, for the
    /// draw-fairness test. Real producers are bounded by the turn structure; this one is not.
    #[arg(long, hide = true, value_name = "COUNT")]
    flood_facts: Option<u64>,

    /// A non-TUI mode (ADR 0032): `radian exec …` is today's `agent` command, the management
    /// commands (`radian memory`/`login`/`config`) run the same library dispatch. Absent: the
    /// full-screen TUI, which is what `radian` has always been.
    #[command(subcommand)]
    command: Option<RadianCommand>,

    /// Test hook, **kept** as a diagnostic: append
    /// `<point> <unix-nanos>` to this file at each step of the terminal-gone exit path. It
    /// tells "the exit path ran but slowly" from "the POLLHUP path never fired" (and, with
    /// the main-loop fallback, shows the fallback firing: a trace that lacks `pollhup` but has
    /// `loop-done`). Inert when the flag is absent: no file, no output, and the input thread's
    /// per-site counters are not even touched (`exit_trace_once` returns on an unset `OnceLock`).
    #[arg(long, hide = true, value_name = "FILE")]
    exit_trace: Option<PathBuf>,

    /// Test hook (`--exit-line-filler <BYTES>`): pad the reason line `main` prints on exit with
    /// this many filler bytes, so a test can make that write exceed a terminal that has stopped
    /// draining (a full pty buffer). It proves the exit line is written with a bound instead of
    /// blocking the process (small-fixes-3). Inert when the flag is absent.
    #[arg(long, hide = true, value_name = "BYTES")]
    exit_line_filler: Option<usize>,
}

/// The subcommands that are not the TUI (ADR 0032). `exec` is today's `agent` command, run
/// through the `agent-cli` library so the two entries cannot drift; the management commands
/// follow in the same shape.
#[derive(clap::Subcommand, Debug)]
enum RadianCommand {
    /// Run one turn or an interactive line session: today's `agent` command, unchanged.
    /// `radian exec -p "…"` prints only the model's text; `radian exec` is the line REPL.
    ///
    /// The `agent` parser owns this mode's flags, help and usage errors (`radian exec --help`
    /// shows them, and a bad flag exits 2 through clap, exactly as `agent` does), so clap's
    /// own help for this variant is off and everything after `exec` is passed through.
    #[command(disable_help_flag = true, disable_version_flag = true)]
    Exec {
        /// Everything `agent` accepts, passed through unchanged.
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "ARGS"
        )]
        args: Vec<String>,
    },

    /// Store an API key for a provider (ADR 0026 D3). The same code `radian exec login` runs.
    Login {
        /// A built-in provider id, or `custom` to name a new endpoint. Omitted: a numbered
        /// list is shown.
        provider: Option<String>,
    },
    /// Remove a stored credential (asks first). The same code `radian exec logout` runs.
    Logout {
        /// The provider id the key is stored under.
        provider: String,
    },
    /// Manage durable memory (ADR 0021 D7): the same grammar as `radian exec memory` and the
    /// TUI's `/memory`.
    Memory {
        /// The workspace project memory is resolved against, like `radian exec --workspace`.
        #[arg(long, value_name = "DIR")]
        workspace: Option<PathBuf>,
        #[command(subcommand)]
        command: radian_host::memory::commands::MemoryCommand,
    },
    /// Configuration tools (ADR 0008): `radian config import-pi …`, the same code
    /// `radian exec config` runs.
    Config {
        #[command(subcommand)]
        command: agent_cli::ConfigCommand,
    },

    /// Theme tools (ADR 0029): `radian theme import <pi-theme.json>`
    /// converts a pi theme to a Radian TOML theme under `~/.radian/themes/`.
    Theme {
        #[command(subcommand)]
        command: ThemeCommand,
    },

    /// View another session read-only (ADR 0043 slice ①): its committed transcript, rendered
    /// with the same blocks a `--resume` uses. No input reaches the session, no socket is
    /// opened, and nothing is written.
    Attach {
        /// The session id: the name of its file in the session directory. An exact id, like
        /// `--resume` (no prefix matching).
        #[arg(value_name = "SESSION_ID")]
        session_id: String,
        /// Keep reading: show the committed events the session's own process appends, until
        /// `q`/Esc/Ctrl-C. Without it the view reads once and stays still.
        #[arg(long)]
        follow: bool,
        /// Where session files live. Defaults to `RADIAN_SESSION_DIR`, then `~/.radian/sessions`
        /// (`$RADIAN_HOME/sessions`), the same rule as the top-level `--session-dir`.
        #[arg(long, value_name = "DIR")]
        session_dir: Option<PathBuf>,
    },

    /// Reserved for ADR 0030's control port: a named agent serving its session over a local
    /// endpoint. Not implemented yet - it refuses with a usage-class exit code.
    Serve,
    /// Reserved for ADR 0030's MCP server mode. Not implemented yet - it refuses with a
    /// usage-class exit code.
    Mcp,
}

/// The `radian theme` subcommands (ADR 0029).
#[derive(clap::Subcommand, Debug)]
enum ThemeCommand {
    /// Convert a pi theme JSON to a Radian TOML theme under `~/.radian/themes/`.
    Import {
        /// The pi theme JSON file to convert.
        #[arg(value_name = "PI-THEME.json")]
        path: PathBuf,
        /// The Radian theme name; default: the file's own `name`, else the file stem.
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// Overwrite an existing `<name>.toml`.
        #[arg(long)]
        force: bool,
    },
}

/// Runs a non-TUI subcommand and returns its exit code (ADR 0032 A3: 0 ok, 1 the run failed,
/// 2 usage/setup, 101 panic, 128+signal - the same codes the `agent` binary returns, because
/// it is the same dispatch).
fn run_subcommand(command: RadianCommand) -> ExitCode {
    match command {
        RadianCommand::Exec { args } => {
            // The agent parser owns `--help`/`--version`/usage errors: `parse_from` exits
            // inside clap for those, exactly as the `agent` binary does.
            // The program name clap shows in usage and errors: `radian exec`, the command
            // the user ran.
            ExitCode::from(agent_cli::parse_and_run(
                std::iter::once("radian exec".to_string()).chain(args),
            ) as u8)
        }
        // The management commands build the same `Cli` the `agent` parser would and run the
        // same dispatch (ADR 0032 direction): `radian login` and `radian exec login` are
        // one code path, not two.
        RadianCommand::Login { provider } => {
            run_agent_cli(agent_cli::Cli::for_command(agent_cli::Command::Login {
                provider,
            }))
        }
        RadianCommand::Logout { provider } => {
            run_agent_cli(agent_cli::Cli::for_command(agent_cli::Command::Logout {
                provider,
            }))
        }
        RadianCommand::Memory { workspace, command } => run_agent_cli(agent_cli::Cli::for_memory(
            workspace.map(|path| path.to_string_lossy().into_owned()),
            command,
        )),
        RadianCommand::Config { command } => {
            run_agent_cli(agent_cli::Cli::for_command(agent_cli::Command::Config {
                command,
            }))
        }
        RadianCommand::Theme { command } => run_theme(command),
        RadianCommand::Attach {
            session_id,
            follow,
            session_dir,
        } => run_attach(session_dir.as_deref(), &session_id, follow),
        // Reserved names (ADR 0032): the shape is fixed now so scripts and docs can use it,
        // the implementation is ADR 0030's. A usage-class refusal (2), never a silent no-op.
        RadianCommand::Serve => reserved("serve"),
        RadianCommand::Mcp => reserved("mcp"),
    }
}

/// Runs `radian theme import` (ADR 0029): a one-time pi → Radian TOML
/// conversion. Exit codes follow ADR 0032 A3 (0 ok, 1 the run failed, 2 usage).
fn run_theme(command: ThemeCommand) -> ExitCode {
    match command {
        ThemeCommand::Import { path, name, force } => {
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(error) => {
                    eprintln!("radian: cannot read {}: {error}", path.display());
                    return ExitCode::from(1);
                }
            };
            let fallback = path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            let converted =
                match radian_tui::theme::importer::convert(&text, name.as_deref(), &fallback) {
                    Ok(converted) => converted,
                    Err(error) => {
                        eprintln!("radian: {}: {error}", path.display());
                        return ExitCode::from(1);
                    }
                };
            let Some(dir) = radian_host::themes_dir() else {
                eprintln!("radian: no themes directory (set RADIAN_HOME or HOME)");
                return ExitCode::from(1);
            };
            match radian_tui::theme::importer::write(&dir, &converted.name, &converted.toml, force)
            {
                Ok(written) => {
                    println!("wrote {}", written.display());
                    for line in &converted.report {
                        println!("{line}");
                    }
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("radian: {error}");
                    ExitCode::from(1)
                }
            }
        }
    }
}

/// Refuses a reserved subcommand: one line on stderr and the usage-class exit code.
fn reserved(name: &str) -> ExitCode {
    eprintln!("radian: `{name}` is reserved for ADR 0030's control port; not implemented yet");
    ExitCode::from(2)
}

/// Runs one management command through the `agent-cli` dispatch and returns its exit code.
fn run_agent_cli(cli: agent_cli::Cli) -> ExitCode {
    ExitCode::from(agent_cli::run_blocking(cli) as u8)
}

/// The TUI's per-turn model call limit when `--max-model-calls` is not given (decision
/// 2026-10-12, dogfooding finding D6).
const TUI_DEFAULT_MAX_MODEL_CALLS: u32 = 1000;

/// How many facts one loop turn processes before it draws. Bounded so a producer that keeps the
/// channel busy cannot starve the frame (ADR 0023 U2's draw fairness): the fairness pty test
/// floods 20 000 facts and still sees at least 100 frames. 64 keeps a turn's work near the
/// pre-refactor 50 ms tick (~50 deltas) while staying far below a frame budget.
const MAX_FACTS_PER_TURN: usize = 64;

/// How many effect rounds one turn may take before the rest carry to the next turn. An
/// effect's result is another event, and a result can ask for one more effect (`@` expansion →
/// send), so the chain is short; the bound keeps one turn's work near a frame budget and hands
/// the rest to the next turn (never dropped, order kept). It is **draw fairness**, not an
/// error: a busy machine with many unrelated events in the channel can legitimately exceed it.
const MAX_EFFECT_ROUNDS: usize = 8;

/// How many consecutive turns a carried effect batch may run before a one-time notice. A
/// genuine infinite effect loop (an effect whose result always produces another effect) would
/// otherwise run every frame; the notice says so and the loop keeps running — a TUI must not
/// exit on it. 1000 turns is far above any real chain (the longest is `@` expansion → send).
const MAX_CARRIED_FRAMES: u64 = 1000;

/// The input thread's poll interval: a wake-up granularity for the hang-up check, not a
/// synchronization device.
const INPUT_POLL: Duration = Duration::from_millis(50);

/// How often the main loop runs its own non-consuming `poll(2)` hang-up check (guard 2 in
/// [`read_input`]'s docs). An idle app wakes 4 times a second and makes one `poll(2)` call per
/// tick; it **does not draw** (the zero-redraw rule), so the cost is one syscall — negligible.
const TTY_CHECK_TICK: Duration = Duration::from_millis(250);

fn mark(label: &str) {
    radian_session::trace::mark(label);
}

/// The `radian` (and `rad` alias) entry point: parse the command line and run it, returning
/// the process exit code. `src/main.rs` and `src/rad.rs` both call this.
pub fn run_cli() -> ExitCode {
    radian_session::trace::start();
    radian_tui::terminal::install_panic_hook();
    let mut cli = Cli::parse();
    // A subcommand is a mode of its own: it never enters the TUI, never reads the TUI's
    // flags, and returns its own exit code (ADR 0032).
    if let Some(command) = cli.command.take() {
        return run_subcommand(command);
    }
    if let Some(path) = &cli.exit_trace {
        let _ = EXIT_TRACE.set(path.clone());
    }
    if let Some(bytes) = cli.exit_line_filler {
        let _ = EXIT_LINE_FILLER.set(bytes);
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("radian: {e}");
            return ExitCode::from(1);
        }
    };
    // The UI's one channel (ADR 0023 U2). It is created here so the signal listener can send
    // into it as well: every source of UI input is an event on this channel.
    let (tx, rx) = std::sync::mpsc::channel::<UiEvent>();
    // SIGTERM/SIGHUP take the ordinary exit path from here on (dogfooding D7).
    let signal_tx = tx.clone();
    if let Err(e) = radian_tui::signals::listen(runtime.handle(), move |signal| {
        let _ = signal_tx.send(UiEvent::Signal(signal));
    }) {
        eprintln!("radian: cannot listen for SIGTERM/SIGHUP: {e}");
        return ExitCode::from(1);
    }
    // A panic must not unwind through a runtime *drop*: that drop can wait on a parked task
    // and hang. Catch it here, shut the runtime down with a bound (which drops the in-flight
    // tool futures, and with them the `kill_on_drop` child processes they own), then exit
    // 101. The panic hook restores the terminal and returns; it no longer exits, because
    // exiting here would skip exactly that cleanup.
    let result =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(cli, &runtime, rx, tx)));
    runtime.shutdown_timeout(Duration::from_secs(2));
    exit_trace("shutdown");
    match result {
        Ok(Ok(reason)) => {
            // A clean exit still says why, on the normal screen (dogfooding D4). `--list`
            // prints its own output and passes `None`.
            if let Some(reason) = reason {
                print_exit_line(&reason);
            }
            // A signal-driven exit is still an orderly one, but the caller asked us to stop:
            // report it the way a shell does, 128 + the signal number.
            let code = match radian_tui::signals::received() {
                Some(signal) => ExitCode::from((128 + signal) as u8),
                None => ExitCode::SUCCESS,
            };
            exit_trace("exit");
            code
        }
        Ok(Err(error)) => {
            print_exit_line(error.message());
            ExitCode::from(error.code())
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
            print_exit_line(&message);
            ExitCode::from(101)
        }
    }
}

/// Why the TUI stopped. ADR 0032 A3 unifies the codes: a **startup refusal** is a
/// usage/setup error and exits 2 (it was 1 before), everything that fails once the run began
/// (the driver, the input, `--list`) exits 1.
enum RunError {
    /// A startup/setup refusal: a bad config, an unknown keybinding, a `--resume` with
    /// nothing to resume, the window check. Exit 2, like clap's own usage errors.
    Startup(String),
    /// The run itself failed. Exit 1.
    Failed(String),
}

/// Every `?` in the startup phase converts a plain message into a [`RunError::Startup`] - the
/// default, because that phase is where nearly every refusal happens. The few failures that
/// can only happen after the run began name [`RunError::Failed`] explicitly.
impl From<String> for RunError {
    fn from(message: String) -> Self {
        Self::Startup(message)
    }
}

impl RunError {
    /// The process exit code (ADR 0032 A3).
    fn code(&self) -> u8 {
        match self {
            Self::Startup(_) => 2,
            Self::Failed(_) => 1,
        }
    }

    /// The one line the normal screen gets.
    fn message(&self) -> &str {
        match self {
            Self::Startup(message) | Self::Failed(message) => message,
        }
    }
}

/// How long `main` waits for the exit line to reach stderr before leaving without it. The write
/// runs on its own short-lived thread (see [`print_exit_line`]); this bound is only reached when
/// the terminal has stopped draining, so the ordinary exit pays nothing for it.
const EXIT_LINE_WAIT: Duration = Duration::from_millis(250);

/// The `--exit-line-filler` value (test hook): unset in production, where the exit line is the
/// reason alone.
static EXIT_LINE_FILLER: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

/// Prints the exit reason on the normal screen, with a **bounded** wait so a terminal that has
/// stopped draining cannot hold the process open.
///
/// `main` reaches here after `runtime.shutdown_timeout`, so the signal task — whose watchdog is
/// the only other way a stuck process ends — is already gone. A plain blocking `writeln!` to a
/// terminal that is not draining (a full pty buffer or XOFF) would then block the process
/// forever. So the write runs on its own short-lived thread and `main` waits at most
/// [`EXIT_LINE_WAIT`]: if it has not finished by then, returning ends the process and the
/// blocked writer with it. The line is best-effort — losing it is not a failure to exit.
///
/// `writeln!`, not `eprintln!`: the latter panics when the terminal is already gone (a closed
/// pty makes the write fail), which would turn a clean exit into 101. A failure to print the
/// line is not a failure to exit.
fn print_exit_line(message: &str) {
    use std::io::Write;
    let mut line = format!("radian: {message}");
    if let Some(filler) = EXIT_LINE_FILLER.get() {
        line.extend(std::iter::repeat_n('x', *filler));
    }
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = writeln!(std::io::stderr(), "{line}");
        let _ = done_tx.send(());
    });
    let _ = done_rx.recv_timeout(EXIT_LINE_WAIT);
}

/// Where the startup diagnostics go.
///
/// A plain run prints them to stderr **before** the alternate screen is entered, where they are
/// harmless (their own scrollback; the comments at the `[config]`/`eprintln!` sites explain the
/// rule). `--pick` (and a valueless `--resume`) enter the alternate screen *before* the session
/// exists, so from then until the session's first frame every startup diagnostic would land on
/// the alternate screen — and the renderer's frame diff skips the cells that did not change, so
/// the text stays drawn where the cursor happened to be (the input box) and never clears (owner
/// 2026-10-17). On that path the lines are collected here and appended to the transcript's
/// startup block, which is where the trust status line already goes (ADR 0031 T3).
struct StartupLog {
    /// `None` before the alternate screen is up (print); `Some` once it is (collect).
    deferred: Option<Vec<String>>,
}

impl StartupLog {
    /// A diagnostic line: printed on the normal screen, collected inside the alternate one.
    fn line(&mut self, text: impl Into<String>) {
        match &mut self.deferred {
            Some(lines) => lines.push(text.into()),
            None => eprintln!("{}", text.into()),
        }
    }

    /// A line the startup block **already** carries (`trust::status_line`): printed on the
    /// normal screen, dropped inside the alternate one, where the block shows it anyway.
    fn print_before_screen(&self, text: &str) {
        if self.deferred.is_none() {
            eprintln!("{text}");
        }
    }

    /// Takes the lines collected inside the alternate screen, in order.
    fn take(&mut self) -> Vec<String> {
        self.deferred.take().unwrap_or_default()
    }
}

/// Writes every decision to the session journal as a `PermissionResolved` (ADR 0006 §5.9).
/// The program name for the resume command: `argv[0]`'s file name, so the line is copyable
/// whatever the binary is called (a `cargo run`, an absolute path, a symlink).
fn program_name() -> String {
    std::env::args_os()
        .next()
        .map(|arg| {
            Path::new(&arg)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| arg.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "radian".to_string())
}

/// Adds every key now in `auth.json` to the session's redaction list. Called after `/login`
/// saves one; `/logout` does **not** call it, because a value
/// that has already appeared must stay redacted.
fn refresh_secrets(secrets: &radian_tui::terminal::SharedSecrets) {
    // Poison-tolerant: a panicked holder of the lock must not stop this append (the list only
    // ever grows, and failing to add a key is the dangerous direction).
    let mut list = secrets
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for key in radian_host::resolved_keys() {
        if !key.is_empty() && !list.contains(&key) {
            list.push(key);
        }
    }
}

/// A finished ChatGPT OAuth flow (browser or device): write the entry, refresh the
/// redaction list on success (the same rule `refresh_secrets` documents for an API key), and
/// turn the error into the one-line `AppInput::LoginSaved` already expects.
fn store_oauth_outcome(
    outcome: Result<radian_host::OAuthTokenResponse, radian_host::OAuthError>,
    secrets: &radian_tui::terminal::SharedSecrets,
) -> Result<(), String> {
    let token = outcome.map_err(|error| error.to_string())?;
    let result = radian_host::AuthStore::open()
        .and_then(|mut store| radian_host::oauth_store_token(&mut store, &token))
        .map_err(|error| error.to_string());
    if result.is_ok() {
        refresh_secrets(secrets);
    }
    result
}

/// One argument of the resume command, quoted when it would not survive a shell.
fn shell_arg(value: &str) -> String {
    if value
        .chars()
        .any(|c| c.is_whitespace() || c == '\'' || c == '"' || c == '\\')
    {
        format!("'{}'", value.replace('\'', "'\\''"))
    } else {
        value.to_string()
    }
}

/// The command a clean exit prints: the program, `--resume <id>`, and only the flags this run
/// was given that must be repeated. A key or a header value is **never** in
/// it — the key comes from the environment and the headers from the config file.
fn resume_command(program: &str, session_id: &str, cli: &Cli, workspace: &Path) -> String {
    let mut parts = vec![
        program.to_owned(),
        "--resume".to_owned(),
        session_id.to_owned(),
    ];
    parts.extend(
        resume_extra_args(cli, workspace)
            .iter()
            .map(|arg| shell_arg(arg)),
    );
    format!("resume: {}", parts.join(" "))
}

/// The extra `--resume` arguments after `--resume <id>`: `--session-dir`, `--workspace` (only
/// when it is not the current directory) and `--config`, as **raw** values. The printed resume
/// command shell-quotes them; herdr's `resume_argv` carries them verbatim (ADR 0034 H3).
fn resume_extra_args(cli: &Cli, workspace: &Path) -> Vec<String> {
    let mut parts = Vec::new();
    if let Some(dir) = &cli.session_dir {
        parts.push("--session-dir".to_owned());
        parts.push(dir.display().to_string());
    }
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|cwd| std::fs::canonicalize(cwd).ok());
    if cli.workspace.is_some() || cwd.as_deref() != Some(workspace) {
        parts.push("--workspace".to_owned());
        parts.push(workspace.display().to_string());
    }
    if let Some(config) = &cli.config {
        parts.push("--config".to_owned());
        parts.push(config.display().to_string());
    }
    parts
}

/// Test hook (`--block-input`): a path whose creation marks "the read is now blocked forever".
/// Set once in `run`; `read_input` writes it and parks.
/// Test hook (`--panic-in-driver`): the driver task panics, so the exit path can be checked.
fn panic_in_driver() {
    panic!("radian: driver task panic test");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR 0027 + ADR 0023 U2: the breath redraws **once per step**, and only while a
    /// spinner is visible. The test walks the injected clock in 50 ms wakes — no wall clock — and
    /// counts the frames the loop's own tick hands out over two breath periods: 32 for an RGB
    /// theme (16 steps each), 8 for a theme that can only switch (one every 500 ms), none idle.
    #[test]
    fn the_breath_redraws_once_per_step_and_only_when_a_spinner_is_visible() {
        /// The frames `spinner_tick` hands the loop while the clock walks two periods in 50 ms
        /// wakes — the same function the loop calls, with the wake's instant as the argument.
        fn frames(app: &mut App) -> usize {
            let base = app.session.spinner_origin;
            let mut last = None;
            let mut count = 0;
            let mut t = Duration::ZERO;
            while t < radian_tui::spinner::PULSE_PERIOD * 2 {
                if spinner_frame_at(app, &mut last, base + t).is_some() {
                    count += 1;
                }
                t += Duration::from_millis(50);
            }
            count
        }

        let mut app = App::new("m", "s");
        app.session
            .projection
            .set_turn(radian_tui::TurnState::Running);
        assert!(app.spinner_visible(), "a running turn shows a spinner");
        app.theme = radian_tui::theme::Theme::dark();
        assert_eq!(
            frames(&mut app),
            usize::from(radian_tui::spinner::PULSE_STEPS) * 2,
            "one frame per step, 16 steps in every 2.0 s period"
        );
        assert_eq!(
            wait_for_an_event(&app),
            radian_tui::spinner::pulse_step(),
            "the loop wakes at the breath's step while it is visible"
        );
        // The 250 ms hang-up tick is still the floor: a slower frame never shortens it further.
        app.theme = radian_tui::theme::Theme::mono();
        assert_eq!(
            frames(&mut app),
            8,
            "a theme that cannot interpolate switches every 500 ms (two colours over 4.0 s)"
        );
        assert_eq!(wait_for_an_event(&app), TTY_CHECK_TICK);
        assert_eq!(
            frames(&mut app),
            8,
            "the walk is a pure function of the clock: the same count again, no wall clock"
        );

        // Idle: the same walk hands out nothing, so an idle screen is never redrawn.
        let mut idle = App::new("m", "s");
        idle.theme = radian_tui::theme::Theme::dark();
        assert_eq!(frames(&mut idle), 0, "no spinner visible, no frame");
        assert_eq!(wait_for_an_event(&idle), TTY_CHECK_TICK);
    }

    /// ADR 0023 U5: a fact-only update (usage, budget, tier, MCP) wakes the UI with a `Dirty`
    /// and the buffer is empty. The loop must still draw a frame, or the status bar would stay
    /// stale until some unrelated event arrived.
    #[test]
    fn a_dirty_notification_with_an_empty_stream_still_draws() {
        let projection = Arc::new(radian_host::SessionProjection::new());
        let mut app = App::new("m", "s");
        app.set_projection(Arc::clone(&projection));
        // The fact-only update: no delta, so the stream snapshot is empty.
        assert!(projection.set_context_window(Some(1000)));
        assert!(
            apply_dirty(&mut app),
            "a fact update must draw a frame even with an empty stream"
        );
        // And a second `Dirty` that finds the flag already cleared is coalesced away.
        assert!(!apply_dirty(&mut app));
    }

    #[test]
    fn the_default_model_call_limit_is_the_tui_one_not_the_cores() {
        let cli = Cli::try_parse_from(["radian", "--model", "m"]).unwrap();
        assert_eq!(cli.max_model_calls, TUI_DEFAULT_MAX_MODEL_CALLS);
        assert_eq!(TUI_DEFAULT_MAX_MODEL_CALLS, 1000);
        let cli =
            Cli::try_parse_from(["radian", "--model", "m", "--max-model-calls", "5"]).unwrap();
        assert_eq!(cli.max_model_calls, 5);
    }

    /// The short aliases shorthand") are the long flags: each
    /// short parses to the same field as its long form, so nothing else about the surface moves.
    #[test]
    fn the_short_flags_parse_to_the_same_fields_as_the_long_ones() {
        let long = Cli::try_parse_from([
            "radian",
            "--model",
            "m",
            "--workspace",
            "/tmp/ws",
            "--yes",
            "--list",
        ])
        .unwrap();
        let short =
            Cli::try_parse_from(["radian", "-m", "m", "-w", "/tmp/ws", "-y", "-l"]).unwrap();
        assert_eq!(short.model, long.model);
        assert_eq!(short.workspace, long.workspace);
        assert_eq!(short.yes, long.yes);
        assert_eq!(short.list, long.list);
        assert_eq!(short.resume, long.resume);
    }

    /// `--resume` takes an optional id (`num_args = 0..=1`); `-r` must behave the same: a bare
    /// `-r` is the picker, and `-r <id>` resumes that id.
    #[test]
    fn the_resume_short_takes_no_value_and_a_value() {
        let long_bare = Cli::try_parse_from(["radian", "--resume"]).unwrap();
        let short_bare = Cli::try_parse_from(["radian", "-r"]).unwrap();
        assert_eq!(short_bare.resume, long_bare.resume);
        assert_eq!(short_bare.resume, Some(None));

        let long_id = Cli::try_parse_from(["radian", "--resume", "abc"]).unwrap();
        let short_id = Cli::try_parse_from(["radian", "-r", "abc"]).unwrap();
        assert_eq!(short_id.resume, long_id.resume);
        assert_eq!(short_id.resume, Some(Some("abc".to_string())));
    }

    /// The `--resume`/`--list` conflict is a property of the flags, not of the spelling: `-r`
    /// rejects `--list` with the same clap error kind as `--resume`, bare or with an id.
    #[test]
    fn the_resume_short_conflicts_with_list_like_the_long_form() {
        for (long, short) in [
            (
                Cli::try_parse_from(["radian", "--resume", "abc", "--list"]),
                Cli::try_parse_from(["radian", "-r", "abc", "--list"]),
            ),
            (
                Cli::try_parse_from(["radian", "--resume", "--list"]),
                Cli::try_parse_from(["radian", "-r", "--list"]),
            ),
        ] {
            let long = long.unwrap_err();
            let short = short.unwrap_err();
            assert_eq!(short.kind(), long.kind());
            assert_eq!(short.kind(), clap::error::ErrorKind::ArgumentConflict);
        }
    }

    /// The loop's `/model` rule (F4, ADR 0027): an **applied route change** carries
    /// the new model's price forward, a change of some other kind leaves it alone, and a route
    /// the catalog cannot price clears it (so later calls are not billed at the old rate).
    #[test]
    fn an_applied_route_change_moves_the_price_and_other_changes_do_not() {
        use radian_session::RuntimeEvent;
        let lookup = |provider: &str, model: &str| match (provider, model) {
            ("opencode-go", "cheap") => Some(radian_host::ModelPrice {
                input: 150_000,
                output: 600_000,
                cache_read: None,
                cache_write: None,
                tiers: Vec::new(),
            }),
            ("anthropic", "expensive") => Some(radian_host::ModelPrice {
                input: 3_000_000,
                output: 15_000_000,
                cache_read: Some(300_000),
                cache_write: Some(3_750_000),
                tiers: Vec::new(),
            }),
            // A model the warm cache does not know.
            _ => None,
        };
        let route_price: RoutePrice = Arc::new(lookup);
        let changed = |change: serde_json::Value| RuntimeEvent::ConfigChanged {
            revision: 1,
            change,
        };

        // A model switch names the new route, so its price is looked up.
        let got = price_for_applied_change(
            &changed(radian_host::ModelRoute::new("opencode-go", "cheap").to_value()),
            &route_price,
        );
        assert_eq!(
            got,
            Some(Some(radian_host::ModelPrice {
                input: 150_000,
                output: 600_000,
                cache_read: None,
                cache_write: None,
                tiers: Vec::new(),
            })),
            "the switched route's price"
        );
        // A route the cache cannot price is **cleared**, not left at the old model's rate.
        assert_eq!(
            price_for_applied_change(
                &changed(radian_host::ModelRoute::new("opencode-go", "unknown").to_value()),
                &route_price
            ),
            Some(None)
        );
        // A non-route change (`/auto`, `/thinking`) is not a price fact at all.
        assert_eq!(
            price_for_applied_change(
                &changed(serde_json::json!({"approval": "auto"})),
                &route_price
            ),
            None
        );
        assert_eq!(
            price_for_applied_change(
                &changed(serde_json::json!({"thinking": "high"})),
                &route_price
            ),
            None
        );
        // Any other runtime event is not either.
        assert_eq!(
            price_for_applied_change(
                &RuntimeEvent::TurnFailed {
                    turn_id: "t-1".into(),
                    message: "boom".into(),
                },
                &route_price
            ),
            None
        );
    }
}
