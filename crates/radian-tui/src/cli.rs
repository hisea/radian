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
use radian_permissions::{AuditSink, PermissionResolved, runtime_event};
use radian_session::SessionJournal;
use radian_tui::app::{AUTO_SCROLL_TICK, App, Effect, Input as AppInput, MemoryResult, PickerData};
use radian_tui::clip::{ClipboardMode, osc52};
use radian_tui::events::{InputEvent, ProjectionSink, UiEvent};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event};
use ratatui::crossterm::terminal;

mod run;
mod tty;

use run::run;

use tty::*;

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

/// How often [`run_attach`] reads: one poll serves the input **and**, with `--follow`, the
/// journal, so a key press and a newly committed line are both seen inside the brief's 250 ms
/// ceiling. Draws are not on this tick: a still frame is not redrawn at all.
const ATTACH_POLL: Duration = Duration::from_millis(50);

/// `radian attach <session> [--follow]` (ADR 0043 slice ①, decision D14): the read-only view
/// of another session's committed facts. It reads the journal file and nothing else — no
/// socket, no token, no write — and the observed session is never told.
///
/// Exit codes follow ADR 0032 A3: `2` for a refusal (an unknown id, no terminal), `0` for a
/// clean quit. The refusal is decided **before** the terminal is entered, so stderr keeps its
/// one line and nothing on disk is touched.
fn run_attach(dir_flag: Option<&Path>, id: &str, follow: bool) -> ExitCode {
    // The id becomes a file name, so it goes through the same rule `--resume` uses
    // (`validate_session_id`): a path-like id must not name a file outside the session
    // directory. The refusal comes before the directory is even resolved, so nothing is
    // looked up, created or read.
    if let Err(error) = radian_host::validate_session_id(id) {
        eprintln!("radian: {error}");
        return ExitCode::from(2);
    }
    // The same resolution as `--resume`: `--session-dir`, then `RADIAN_SESSION_DIR`, then the
    // data directory (ADR 0020).
    let dir = match radian_host::session_dir(dir_flag) {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("radian: {error}");
            return ExitCode::from(2);
        }
    };
    let path = dir.join(format!("{id}.jsonl"));
    if !path.exists() {
        eprintln!("radian: no session {id:?} in {}", dir.display());
        return ExitCode::from(2);
    }
    let mut view = match radian_tui::attach::AttachView::open(&path, id) {
        Ok(view) => view,
        Err(error) => {
            eprintln!("radian: {error}");
            return ExitCode::from(2);
        }
    };
    // No mouse reporting: this view has no mouse features, so the terminal keeps its native
    // selection and copy.
    let (mut guard, mut terminal) = match enter_terminal(false) {
        Ok(pair) => pair,
        Err(error) => {
            eprintln!("radian: {error}");
            return ExitCode::from(2);
        }
    };
    let mut dirty = true;
    let mut failure: Option<String> = None;
    loop {
        if dirty {
            if let Err(error) = terminal.draw(|frame| view.render(frame)) {
                // The terminal is gone (the pty master closed): leave the ordinary way.
                failure = Some(error.to_string());
                break;
            }
            dirty = false;
        }
        match read_input(ATTACH_POLL) {
            Input::Gone => break,
            Input::Idle => {
                if follow {
                    match view.poll() {
                        // Durable facts only: the frame changes when a message was committed,
                        // a replacement landed or the turn's boundary moved.
                        Ok(changed) => dirty |= changed,
                        Err(error) => {
                            failure = Some(error);
                            break;
                        }
                    }
                }
            }
            Input::Event(Event::Key(key)) => match view.on_key(&key) {
                radian_tui::attach::AttachKey::Quit => break,
                radian_tui::attach::AttachKey::Redraw => dirty = true,
                // Swallowed: no input event, no command, no write (ADR 0043 D14).
                radian_tui::attach::AttachKey::Ignored => {}
            },
            // A resize re-lays the transcript out on the next draw.
            Input::Event(Event::Resize(..)) => dirty = true,
            Input::Event(_) => {}
        }
    }
    guard.restore();
    match failure {
        None => ExitCode::SUCCESS,
        Some(error) => {
            eprintln!("radian: {error}");
            ExitCode::from(1)
        }
    }
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

/// The TUI's herdr reporter (ADR 0034): the sender thread, the monotonic `seq`, the session
/// identity, the resume command and the last reported state. `maybe_report` recomputes the
/// state and queues a report only when it changed; the UI thread never waits on I/O.
struct HerdrReporter {
    reporter: radian_host::herdr::Reporter,
    seq: radian_host::herdr::Seq,
    session: radian_host::herdr::SessionIdentity,
    resume: Option<Vec<String>>,
    last: Option<(radian_host::herdr::State, Option<String>)>,
}

impl HerdrReporter {
    fn maybe_report(&mut self, app: &App) {
        let (turn, pending, trust) = app.herdr_state_inputs();
        let state = radian_host::herdr::state(turn, pending, trust);
        let message = (state == radian_host::herdr::State::Blocked)
            .then(|| radian_host::herdr::blocked_message(app.herdr_blocked().as_ref(), trust));
        if self.last.as_ref() == Some(&(state, message.clone())) {
            return;
        }
        self.last = Some((state, message.clone()));
        let mut report = radian_host::herdr::Report::new(state);
        if let Some(message) = message {
            report = report.message(message);
        }
        report = report.session(self.session.id.clone(), self.session.path.clone());
        if let Some(argv) = &self.resume {
            report = report.resume(Some(argv.clone()));
        }
        self.reporter.report(&report, self.seq.next());
    }

    fn release(&self) {
        self.reporter.release(self.seq.next());
    }
}

/// Builds the reporter when the environment says we are in a herdr pane (ADR 0034 H1). The
/// returned `Option<String>` is the reason the resume command was left out, for the startup
/// line.
fn herdr_reporter(
    config: &radian_host::config::Config,
    session_id: &str,
    journal: &Path,
    extra: &[String],
) -> Option<(HerdrReporter, Option<String>)> {
    let env = radian_host::herdr::detect(config.integrations.herdr)?;
    let (resume, note) = match radian_host::herdr::resume_argv(session_id, extra) {
        Ok(argv) => (Some(argv), None),
        Err(reason) => (None, Some(reason)),
    };
    Some((
        HerdrReporter {
            reporter: radian_host::herdr::Reporter::spawn(env),
            seq: radian_host::herdr::Seq::new(),
            session: radian_host::herdr::SessionIdentity {
                id: session_id.to_owned(),
                path: journal.to_path_buf(),
            },
            resume,
            last: None,
        },
        note,
    ))
}

/// Test hook (`--block-input`): a path whose creation marks "the read is now blocked forever".
/// Set once in `run`; `read_input` writes it and parks.
/// Test hook (`--panic-in-driver`): the driver task panics, so the exit path can be checked.
fn panic_in_driver() {
    panic!("radian: driver task panic test");
}

/// Looks up a route's catalog prices from the warm local cache (ADR 0026 D7, ADR 0027
/// revision 1). Offline by construction: a switch never fetches, and a miss means "no price".
/// The host owns the type because the resume fold prices historical calls with the same rule.
type RoutePrice = radian_host::cost::RoutePrice;

/// The catalog prices of the route an **applied** `ConfigChanged` names (ADR 0026 D5/D7).
///
/// Pure and total: every other runtime event, every non-model change (`{"approval": …}`,
/// `{"thinking": …}`) and every change whose route is not resolvable yields `None`, so a
/// caller can call it unconditionally. Extraction is what makes the loop's rule unit-testable
/// without a pty: "a switch re-prices the calls **after** it, and only those".
fn price_for_applied_change(
    event: &radian_session::RuntimeEvent,
    route_price: &RoutePrice,
) -> Option<Option<radian_host::ModelPrice>> {
    let radian_session::RuntimeEvent::ConfigChanged { change, .. } = event else {
        return None;
    };
    let route = radian_host::ModelRoute::from_value(change)?;
    // `Some(None)` is "the route changed but the catalog has no price for it": the caller must
    // still clear the old price, or later calls would be billed at the previous model's rate.
    Some(route_price(&route.provider, &route.model))
}

struct LoopContext<'a> {
    /// The startup route (ADR 0026 D11): the fallback when the session has no route yet.
    startup_route: (String, String),
    /// The catalog prices of a route, from the **warm local cache only** (ADR 0026 D7: a
    /// switch never fetches). Resolved on startup and again whenever a route change is
    /// applied, so every later call is billed at its own model's price (ADR 0027).
    route_price: RoutePrice,
    /// The config's `[providers.<name>]` declarations (ADR 0026 D10), for `/model`'s provider
    /// completion.
    config_providers: std::collections::BTreeMap<String, radian_host::ProviderDecl>,
    /// The applied thinking level (ADR 0026 D11), kept in step with the app, for `/thinking`'s
    /// current-value marker.
    current_thinking: String,
    /// The configured theme name, for `/theme`'s current-value marker.
    current_theme: Option<String>,
    /// The request's `--max-tokens`: a level whose budget cannot fit is not offered and is
    /// clamped past (review B1).
    thinking_max_tokens: u32,
    workspace: &'a Path,
    runtime: &'a tokio::runtime::Runtime,
    /// The UI's one handle on the session (ADR 0023 U5): commands go through it, and it
    /// re-validates the capability the UI declared.
    session: &'a Arc<dyn radian_host::SessionFacade>,
    memory: &'a Option<MemoryService>,
    host: &'a SessionHost,
    tx: &'a Sender<UiEvent>,
    /// The pending queue (ADR 0023 U7): the loop reads the head and the count when it is told
    /// something changed, and the facade answers it.
    queue: &'a radian_host::SessionQueue,
    /// The signal that stopped the loop, if any: it decides the reason line (and, in `main`,
    /// the exit code).
    signal: Option<i32>,
    /// How a selection copy reaches the clipboard (`tui.clipboard`), read once from the app
    /// when the context is built: the mode cannot change mid-run.
    clipboard: ClipboardMode,
    /// The herdr reporter (ADR 0034), when running inside a herdr pane.
    herdr: Option<HerdrReporter>,
    /// The sandbox the user's `!`/`!!` commands run in (ADR 0033): the coding tools' own,
    /// cloned. `None` on the `safe` tier (no shell).
    bash_sandbox: Option<radian_sandbox::Sandbox>,
    /// The running `!` command's cancellation token (Esc, ADR 0033).
    bash_cancel: Option<agent_core::CancellationToken>,
    /// The opencode-go usage gate (ADR 0027): the UI flips it when `/model`
    /// switches the current provider. `None` when there is no key or the period is off.
    usage_gate: Option<tokio::sync::watch::Sender<bool>>,
    /// The chatgpt usage gate (ADR 0026), same mechanism, flipped to
    /// `route.provider == "chatgpt"` instead.
    chatgpt_usage_gate: Option<tokio::sync::watch::Sender<bool>>,
}

impl LoopContext<'_> {
    /// Feeds one event to the app. Returns whether the frame is dirty; the effects it produced
    /// are appended to `effects`.
    fn handle(&mut self, event: UiEvent, app: &mut App, effects: &mut Vec<Effect>) -> bool {
        // ADR 0027: the loop is the only place that reads the wall clock for the
        // footer's `speed`/`elapsed`; the timer itself is pure.
        app.set_now(std::time::Instant::now());
        // ADR 0027: a `/model` switch arrives as a model `ConfigChanged` at the turn
        // boundary. Flip the usage gate then: away from opencode-go the contributor stops
        // refreshing (no network) and its row hides; back to it, one immediate refresh.
        if let UiEvent::Runtime(radian_session::RuntimeEvent::ConfigChanged { change, .. }) = &event
            && let Some(route) = radian_host::ModelRoute::from_value(change)
        {
            if let Some(gate) = &self.usage_gate {
                let _ = gate.send(route.provider == "opencode-go");
            }
            if let Some(gate) = &self.chatgpt_usage_gate {
                let _ = gate.send(chatgpt_usage_active(&route.provider));
            }
        }
        let result = match event {
            UiEvent::Agent(event) => app.update(AppInput::Event(event)),
            UiEvent::Runtime(event) => {
                // ADR 0027 + ADR 0026 D5: the driver applies a queued route at a
                // turn boundary and tells us through `ConfigChanged`. From now on every call
                // is the new model's, so the price that prices it changes here — earlier
                // calls keep the fee they already accumulated.
                if let Some(price) = price_for_applied_change(&event, &self.route_price) {
                    app.set_model_price(price);
                }
                app.update(AppInput::Runtime(event))
            }
            UiEvent::Pending => {
                // ADR 0023 U7: the UI shows the queue's head. Reading it here is what makes a
                // waiter that gave up (a cancelled turn) disappear immediately.
                let head = self.queue.head();
                app.update(AppInput::Pending(head.map(Box::new), self.queue.len()))
            }
            UiEvent::Command(input) => app.update(input),
            UiEvent::Input(InputEvent::Key(key)) => app.on_key(key),
            UiEvent::Input(InputEvent::Paste(text)) => app.update(AppInput::Paste(text)),
            UiEvent::Input(InputEvent::Resize { rows, cols }) => {
                app.update(AppInput::Resize { rows, cols })
            }
            UiEvent::Input(InputEvent::MouseScroll(direction)) => app.mouse_scroll(direction),
            UiEvent::Input(InputEvent::MouseDown { row, col }) => {
                app.update(AppInput::MouseDown { row, col })
            }
            UiEvent::Input(InputEvent::MouseDrag { row, col }) => {
                app.update(AppInput::MouseDrag { row, col })
            }
            UiEvent::Input(InputEvent::MouseUp { row, col }) => {
                app.update(AppInput::MouseUp { row, col })
            }
            UiEvent::Dirty => return apply_dirty(app),
            UiEvent::Signal(signal) => {
                // Leave through the ordinary clean-exit path: the terminal is restored, the
                // driver is stopped, and `main` prints the reason (a signal exits
                // 128+signal). No last frame: after a SIGHUP the terminal may already be gone.
                self.signal = Some(signal);
                app.quit = true;
                return false;
            }
            UiEvent::TerminalGone => {
                // The terminal went away (the pty master closed, the ssh session dropped):
                // leave through the ordinary clean-exit path. No signal was received, so the
                // exit code is the ordinary 0.
                exit_trace("terminal-gone");
                app.quit = true;
                return false;
            }
            UiEvent::DriverStopped(end) => {
                // The driver stopped: a fatal error (ADR 0001: the journal may be broken, so
                // no record is written), a panic, or a graceful close. The session is over.
                // The TUI leaves `Running`, names the reason in the status bar and refuses
                // `/fork` (ADR 0022 §1's "idle session" no longer exists). It stays so the
                // reason is readable; the user quits with the ordinary keys.
                let reason = match &end {
                    radian_host::DriverEnd::Ok => "the driver stopped".to_string(),
                    radian_host::DriverEnd::Failed(error) => format!("driver failed: {error}"),
                    radian_host::DriverEnd::Panicked(message) => {
                        format!("driver panicked: {message}")
                    }
                };
                app.session.session_ended = Some(reason.clone());
                app.session.pending_fork = None;
                app.session.projection.set_turn(radian_tui::TurnState::Idle);
                app.session.note = Some(reason.clone());
                app.session.entries.push(radian_tui::Entry::new(
                    radian_tui::EntryKind::Notice,
                    format!("the session ended: {reason}"),
                ));
                return true;
            }
        };
        // The completion menu's current-value markers read these; keep them in step with the
        // app (the footer's `reasoning` item is the same value).
        if let Some(level) = app.session.thinking_level.clone() {
            self.current_thinking = level;
        }
        effects.extend(result.effects);
        result.dirty
    }

    /// Runs one effect. Its result is an event on the channel (ADR 0023 U2), so the caller
    /// picks it up on the next round.
    fn execute(&mut self, effect: Effect) -> Result<(), String> {
        match effect {
            // ADR 0023 U5: the UI asks the session through its facade; the facade declares
            // what it can do and re-checks when the command runs.
            Effect::SendFollowup(text) => {
                self.command(radian_host::SessionCommand::SendFollowup(text))
            }
            Effect::Steer(text) => self.command(radian_host::SessionCommand::Steer(text)),
            Effect::Cancel => {
                // Ctrl-C cancels the turn; it also stops a running `!` (ADR 0033) so there is
                // no orphan process group.
                if let Some(cancel) = self.bash_cancel.take() {
                    cancel.cancel();
                }
                self.command(radian_host::SessionCommand::Cancel)
            }
            Effect::Quit => Ok(()),
            // ADR 0033: run the `!`/`!!` command through radian-coding's sandboxed executor
            // (`run_user_bash`, the shell tool's own path), off the loop so Esc can cancel it.
            Effect::RunBash {
                command,
                exclude_from_context,
            } => {
                let tx = self.tx.clone();
                let sandbox = self.bash_sandbox.clone();
                let workspace = self.workspace.to_path_buf();
                let cancel = agent_core::CancellationToken::new();
                self.bash_cancel = Some(cancel.clone());
                self.runtime.spawn(async move {
                    let result = match sandbox {
                        Some(sandbox) => {
                            radian_coding::run_user_bash(&workspace, &sandbox, &command, &cancel)
                                .await
                        }
                        None => Err("the `safe` tier has no shell (ADR 0012); `!` needs a \
                                     shell-capable tier"
                            .to_owned()),
                    };
                    let _ = tx.send(UiEvent::Command(AppInput::BashDone {
                        command,
                        exclude_from_context,
                        result,
                    }));
                });
                Ok(())
            }
            // ADR 0033 + ADR 0011: the `!` message is injected without waking an idle driver.
            Effect::Inject(message) => self.command(radian_host::SessionCommand::Inject(message)),
            // ADR 0033: the durable audit record, for `!` and `!!` alike.
            Effect::AuditUserBash {
                command,
                exclude_from_context,
                exit_code,
                cancelled,
            } => {
                self.session.record_user_bash(&radian_host::UserBashRecord {
                    command,
                    exclude_from_context,
                    exit_code,
                    cancelled,
                });
                Ok(())
            }
            Effect::CancelBash => {
                if let Some(cancel) = self.bash_cancel.take() {
                    cancel.cancel();
                }
                Ok(())
            }
            // ADR 0008 revision: `/theme` loads the theme and writes `tui.theme` back. Both
            // are I/O, so they run here and the result comes back as `Input::ThemeReady`.
            Effect::Theme { name } => {
                let outcome = radian_tui::theme::switch(
                    &name,
                    radian_tui::theme::Theme::no_color_env(),
                    radian_host::themes_dir().as_deref(),
                    &self.host.theme_colors,
                    self.host.theme_write_path.as_deref(),
                    self.host.config_read_only,
                );
                if outcome.theme.is_some() {
                    self.current_theme = Some(name);
                }
                self.send(AppInput::ThemeReady {
                    theme: outcome.theme.map(Box::new),
                    notices: outcome.notices,
                });
                Ok(())
            }
            // ADR 0026 D9: the in-session switch. The facade applies it to the hooks (which
            // audit it) and queues the durable `ConfigChanged` for the next turn boundary.
            Effect::SetApprovalPolicy { on } => self
                .session
                .set_approval(if on {
                    radian_permissions::ApprovalPolicy::Auto
                } else {
                    radian_permissions::ApprovalPolicy::OnAsk
                })
                .map_err(|refused| {
                    format!(
                        "cannot switch auto: {} ({})",
                        refused.reason,
                        refused.capability.as_deref().unwrap_or("capability")
                    )
                }),
            // ADR 0038 slice 2: the run-time tier switch. The facade rebuilds the profile and
            // the sandbox (a failed probe is a refusal that changes nothing) and queues the
            // durable `ConfigChanged`; when the user chose "remember", the loop also writes the
            // per-project record. The answer comes back as `Input::AccessApplied`.
            Effect::SetAccess { tier, remember } => {
                match self.session.set_access(tier.preset()) {
                    Ok(()) => {
                        if remember
                            && let Err(error) =
                                radian_host::access::remember_for(self.workspace, tier.preset())
                        {
                            self.send(AppInput::SessionDone(vec![format!(
                                "access: switched, but could not remember it for this project: \
                                 {error}"
                            )]));
                        }
                        self.send(AppInput::AccessApplied {
                            tier,
                            remember,
                            result: Ok(()),
                        });
                    }
                    Err(refused) => {
                        self.send(AppInput::AccessApplied {
                            tier,
                            remember,
                            result: Err(refused.reason),
                        });
                    }
                }
                Ok(())
            }
            Effect::AnswerRequest {
                request_id,
                response,
            } => {
                // ADR 0023 U5: the answer goes through the facade, which re-validates the
                // `AnswerRequest` capability. A late answer is silently ignored there.
                match self.session.answer(request_id, response) {
                    Ok(()) => Ok(()),
                    Err(refused) if refused.capability.is_some() => {
                        self.send(AppInput::CommandRefused(refused));
                        Ok(())
                    }
                    Err(refused) => Err(refused.reason),
                }
            }
            // `@` expansion and completion read files, so they are effects; their results go
            // back through the channel as `Command` events.
            Effect::ExpandMentions { text } => {
                let result = radian_tui::mention::expand(&text, self.workspace);
                self.send(AppInput::MentionsExpanded(result));
                Ok(())
            }
            Effect::CompleteMention { text, cursor } => {
                let result = radian_tui::mention::complete(&text, cursor, self.workspace);
                self.send(AppInput::CompletionReady(result));
                Ok(())
            }
            Effect::FetchCompletions(fetch) => {
                // A level that needs the host: list the candidates here (I/O) and send them back as
                // `Input::CompletionsReady` (ADR 0023 U3: `update` never does I/O).
                let candidates = match &fetch {
                    radian_tui::command::Fetch::MemoryRecords { command, .. } => {
                        memory_record_candidates(self.memory, command)
                    }
                    radian_tui::command::Fetch::Themes { .. } => {
                        // The current theme is marked, so the menu says what a pick changes.
                        let current = self.current_theme.clone();
                        radian_tui::command::theme_candidates(radian_host::themes_dir().as_deref())
                            .into_iter()
                            .map(|mut candidate| {
                                if Some(&candidate.insert) == current.as_ref() {
                                    candidate.current = true;
                                }
                                candidate
                            })
                            .collect()
                    }
                    radian_tui::command::Fetch::ModelRoute { .. } => {
                        // Mark the route in effect (the slot, or the startup route before it).
                        let (current_provider, current_model) = self
                            .session
                            .model_route()
                            .unwrap_or_else(|| self.startup_route.clone());
                        model_route_candidates(
                            Some(current_provider.as_str()),
                            &self.config_providers,
                        )
                        .into_iter()
                        .map(|mut candidate| {
                            let is_current = candidate.insert
                                == format!("{current_provider}/{current_model}")
                                || candidate.insert == format!("{current_provider}/");
                            if is_current {
                                candidate.current = true;
                            }
                            candidate
                        })
                        .collect()
                    }
                    radian_tui::command::Fetch::ThinkingLevels { .. } => {
                        // D11: only the levels the current model supports. Each row names the
                        // wire value it sends (`enabled:4096` for Anthropic, `high` for
                        // openai-compatible), and the applied level is marked.
                        let (route_provider, route_model) = self
                            .session
                            .model_route()
                            .unwrap_or_else(|| self.startup_route.clone());
                        let provider_ref =
                            provider_ref_for(&route_provider, &self.config_providers);
                        radian_tui::command::thinking_candidates(
                            &provider_ref,
                            &route_model,
                            self.thinking_max_tokens,
                            &self.current_thinking,
                        )
                    }
                    // `/skill:`'s candidates: the discovered skill names, including the ones
                    // with `disable-model-invocation: true` — that flag hides a skill from the
                    // *model*, not from the user (ADR 0025 S3; pi registers every loaded skill
                    // as a `skill:<name>` command).
                    radian_tui::command::Fetch::SkillNames { .. } => {
                        radian_host::completion_candidates(self.session.skills())
                            .into_iter()
                            .map(|(name, description)| radian_tui::command::Candidate {
                                insert: name,
                                description,
                                current: false,
                            })
                            .collect()
                    }
                    // `/logout`'s candidates: every stored credential (D3).
                    radian_tui::command::Fetch::LoggedInProviders { .. } => {
                        match radian_host::list_auth() {
                            Ok(entries) => entries
                                .into_iter()
                                .map(|entry| radian_tui::command::Candidate {
                                    insert: entry.provider_id,
                                    description: match entry.kind.as_str() {
                                        "api_key" if entry.usable => "stored api key".to_owned(),
                                        "api_key" => "stored (unusable here)".to_owned(),
                                        other => format!("stored {other} entry"),
                                    },
                                    current: false,
                                })
                                .collect(),
                            Err(error) => {
                                vec![radian_tui::command::Candidate {
                                    insert: String::new(),
                                    description: format!("cannot read auth.json: {error}"),
                                    current: false,
                                }]
                            }
                        }
                    }
                };
                self.send(AppInput::CompletionsReady { fetch, candidates });
                Ok(())
            }
            Effect::RunMemoryCommand { command, confirmed } => {
                let result = match self.memory {
                    Some(service) => {
                        let json = command.json();
                        match radian_host::memory::commands::execute(
                            service,
                            command.clone(),
                            confirmed,
                            json,
                        ) {
                            Ok(output) => match output.needs_confirmation {
                                Some(prompt) => MemoryResult::NeedsConfirmation { command, prompt },
                                None => MemoryResult::Output(radian_tui::command::output_notices(
                                    &output,
                                )),
                            },
                            Err(error) => MemoryResult::Failed(error.message),
                        }
                    }
                    None => {
                        MemoryResult::Failed("memory is disabled (no RADIAN_HOME or HOME)".into())
                    }
                };
                self.send(AppInput::MemoryCommandDone(result));
                Ok(())
            }
            // `/skill:<name> [args]` (ADR 0025 S3): the host reads the body and folds it into
            // the message; the app decides follow-up vs steer.
            Effect::ExpandSkill { name, args, steer } => {
                let result = self.session.expand_skill(&name, &args);
                self.send(AppInput::SkillExpanded { result, steer });
                Ok(())
            }
            // `/skills` (ADR 0025 S3): the discovered skills as notices.
            Effect::ListSkills => {
                let notices = radian_host::notices(self.session.skills());
                self.send(AppInput::SessionDone(notices));
                Ok(())
            }
            // ADR 0037 decision 4: `/jobs` lists or stops the session's background jobs. The
            // registry lives in the host, so the loop reads it here and shows notices.
            Effect::Jobs(request) => {
                let notices = match request {
                    radian_tui::command::JobsRequest::List => {
                        let jobs = self.session.jobs();
                        if jobs.is_empty() {
                            vec!["no background jobs in this session".to_owned()]
                        } else {
                            jobs.iter()
                                .map(|job| {
                                    format!(
                                        "{}  {}  {}s  {}  {}",
                                        job.id,
                                        job.state.label(),
                                        job.elapsed.as_secs(),
                                        job.command,
                                        job.output
                                    )
                                })
                                .collect()
                        }
                    }
                    radian_tui::command::JobsRequest::Kill(id) => {
                        match self.session.kill_job(&id) {
                            Ok(()) => {
                                vec![format!("stopping {id} (SIGTERM, then SIGKILL after 2s)")]
                            }
                            Err(error) => vec![error],
                        }
                    }
                    radian_tui::command::JobsRequest::KillAll => {
                        let running = self
                            .session
                            .jobs()
                            .iter()
                            .filter(|job| job.state.is_running())
                            .count();
                        self.session.kill_all_jobs();
                        vec![format!("stopping {running} background job(s)")]
                    }
                };
                self.send(AppInput::SessionDone(notices));
                Ok(())
            }
            // ADR 0031 T3: apply the startup trust choice through the facade (it writes
            // `trust.json` and journals `ProjectTrustChanged`), then show the new state.
            Effect::SetTrust { choice } => {
                match self.session.record_trust(choice) {
                    Ok(setup) => self.send(AppInput::TrustResolved(setup)),
                    Err(error) => {
                        self.send(AppInput::CommandRefused(radian_host::CommandRefused {
                            command: "trust".to_owned(),
                            reason: error,
                            capability: None,
                        }))
                    }
                }
                Ok(())
            }
            Effect::Compact { instructions } => {
                // The facade runs the request between turns and reports the outcome here; the
                // result becomes an event like every other effect result.
                let tx = self.tx.clone();
                self.session.compact(
                    instructions,
                    Box::new(move |result| {
                        let _ = tx.send(UiEvent::Command(AppInput::CompactDone(result)));
                    }),
                );
                Ok(())
            }
            Effect::ModelSwitch { provider, model } => {
                // D6: the target needs a credential. `resolve_key` reads `auth.json` (file I/O,
                // fine in the loop): without one the switch dies here with the `/login` hint.
                // `resolve_key` only resolves an `api_key` entry; `chatgpt`'s login is an `oauth`
                // entry (ADR 0026), so `has_login` - the presence check both shapes
                // share, as at startup and in `RouteContext::args_for` - also counts here.
                let checked = match radian_host::auth::resolve_key(&provider) {
                    Ok(found) if found.is_some() || radian_host::has_login(&provider) => {
                        // D7: `B'` from the warm catalog, `T` from the projection's last report.
                        let budget = self.session.budget_for_route(&provider, &model);
                        let tokens = self
                            .session
                            .projection()
                            .budget()
                            .map(|report| report.projected_tokens);
                        AppInput::ModelChecked {
                            provider,
                            model,
                            tokens,
                            budget,
                        }
                    }
                    Ok(_) => {
                        self.send(AppInput::CommandRefused(radian_host::CommandRefused {
                            command: format!("/model {provider}/{model}"),
                            reason: format!(
                                "no credential for `{provider}`: run /login {provider}"
                            ),
                            capability: None,
                        }));
                        return Ok(());
                    }
                    Err(error) => {
                        self.send(AppInput::CommandRefused(radian_host::CommandRefused {
                            command: format!("/model {provider}/{model}"),
                            reason: error.to_string(),
                            capability: None,
                        }));
                        return Ok(());
                    }
                };
                self.send(checked);
                Ok(())
            }
            Effect::CompactForSwitch {
                provider,
                model,
                budget,
            } => {
                // D7: the same compaction path as `/compact`, targeting the new model's B'.
                let tx = self.tx.clone();
                self.session.compact_with_budget(
                    None,
                    Some(budget),
                    Box::new(move |result| {
                        let _ = tx.send(UiEvent::Command(AppInput::SwitchCompacted {
                            provider,
                            model,
                            result,
                        }));
                    }),
                );
                Ok(())
            }
            Effect::SetThinking { level } => {
                // D11: clamp the requested level against the **current** route's map; the
                // route comes from the slot (a resumed change or a queued switch included).
                let (route_provider, route_model) = self
                    .session
                    .model_route()
                    .unwrap_or_else(|| self.startup_route.clone());
                let provider_ref = provider_ref_for(&route_provider, &self.config_providers);
                let supported: Vec<String> = radian_host::supported_thinking_levels_ref_with(
                    &provider_ref,
                    &route_model,
                    Some(self.thinking_max_tokens),
                )
                .into_iter()
                .map(str::to_owned)
                .collect();
                let map = radian_host::thinking_map_for_ref(&provider_ref, &route_model);
                let applied =
                    radian_host::clamp_thinking_with(map, &level, Some(self.thinking_max_tokens))
                        .map(|(level, _)| level.to_string());
                self.send(AppInput::ThinkingChecked {
                    requested: level,
                    applied,
                    supported,
                });
                Ok(())
            }
            Effect::QueueThinking { level } => {
                // D11: the change is the level name only — no key, no header values.
                self.session
                    .queue_config(serde_json::json!({ "thinking": level }));
                Ok(())
            }
            Effect::QueueModel { provider, model } => {
                // D5: the route is the whole change — no key, no header values.
                self.session
                    .queue_config(radian_host::ModelRoute::new(provider, model).to_value());
                Ok(())
            }
            Effect::Session(request) => {
                let notices = handle_session_request(self.host, self.runtime, request);
                self.send(AppInput::SessionDone(notices));
                Ok(())
            }
            Effect::OpenPicker { query } => {
                self.send(load_picker(self.host, query));
                Ok(())
            }
            // `/login`'s verification request (ADR 0026 D3): one GET, off the UI thread; the
            // answer comes back as `Input::LoginVerified`. The key rides in the closure only —
            // it is never logged here.
            Effect::LoginVerify {
                name,
                protocol,
                base_url,
                key,
            } => {
                let protocol = radian_host::Protocol::from_kind(protocol)
                    .unwrap_or(radian_host::Protocol::OpenAiCompatible);
                let tx = self.tx.clone();
                self.runtime.spawn(async move {
                    let result = radian_host::login::verify(protocol, &base_url, &key).await;
                    let _ = tx.send(UiEvent::Command(AppInput::LoginVerified { name, result }));
                });
                Ok(())
            }
            // The save: the only place `/login` writes, and it is `auth.json` (D3 — the
            // config file stays read-only).
            Effect::LoginSave { name, key, env } => {
                let env: std::collections::BTreeMap<String, String> = env.into_iter().collect();
                let result = radian_host::AuthStore::open()
                    .and_then(|mut store| store.upsert_api_key(&name, &key, &env))
                    .map_err(|error| error.to_string());
                if result.is_ok() {
                    // The session's redaction list must know the new key from now on
                    //: the panic log and an export read it live.
                    refresh_secrets(&self.host.secrets);
                }
                self.send(AppInput::LoginSaved { name, result });
                Ok(())
            }
            // Sign in with ChatGPT, browser flow (ADR 0026): bind the listener,
            // report the URL right away (`LoginOAuthUrlReady`) so a remote/headless session —
            // or a browser the `open`/`xdg-open` guess failed to launch — has something to
            // copy, then wait for the redirect and exchange. The whole thing is one spawned
            // task so a slow or abandoned browser step cannot block the UI loop; it ends on
            // the same `LoginSaved` event the API-key path does.
            Effect::LoginOAuthBrowser => {
                let tx = self.tx.clone();
                let secrets = Arc::clone(&self.host.secrets);
                self.runtime.spawn(async move {
                    let endpoints = radian_host::OAuthEndpoints::default();
                    let (login, listener) = match radian_host::BrowserLogin::start(&endpoints).await
                    {
                        Ok(pair) => pair,
                        Err(error) => {
                            let _ = tx.send(UiEvent::Command(AppInput::LoginSaved {
                                name: radian_host::CHATGPT_PROVIDER_ID.to_owned(),
                                result: Err(error.to_string()),
                            }));
                            return;
                        }
                    };
                    let _ = tx.send(UiEvent::Command(AppInput::LoginOAuthUrlReady {
                        url: login.authorize_url.clone(),
                    }));
                    radian_host::oauth_chatgpt::try_open_browser(&login.authorize_url);
                    let outcome = login
                        .finish(
                            &endpoints,
                            listener,
                            radian_host::oauth_chatgpt::CALLBACK_TIMEOUT,
                        )
                        .await;
                    let result = store_oauth_outcome(outcome, &secrets);
                    let _ = tx.send(UiEvent::Command(AppInput::LoginSaved {
                        name: radian_host::CHATGPT_PROVIDER_ID.to_owned(),
                        result,
                    }));
                });
                Ok(())
            }
            // Sign in with ChatGPT, device-code flow: ask for a code, report it
            // (`LoginOAuthDeviceReady`), then poll in the same spawned task.
            Effect::LoginOAuthDevice => {
                let tx = self.tx.clone();
                let secrets = Arc::clone(&self.host.secrets);
                self.runtime.spawn(async move {
                    let endpoints = radian_host::OAuthEndpoints::default();
                    let start = match radian_host::oauth_device_start(&endpoints).await {
                        Ok(start) => start,
                        Err(error) => {
                            let _ = tx.send(UiEvent::Command(AppInput::LoginSaved {
                                name: radian_host::CHATGPT_PROVIDER_ID.to_owned(),
                                result: Err(error.to_string()),
                            }));
                            return;
                        }
                    };
                    let _ = tx.send(UiEvent::Command(AppInput::LoginOAuthDeviceReady {
                        user_code: start.user_code.clone(),
                        verification_url: radian_host::oauth_chatgpt::DEVICE_VERIFICATION_URL
                            .to_owned(),
                    }));
                    // 5 minutes at the server's own interval (floored at
                    // `DEVICE_POLL_MIN_INTERVAL`), matching the browser flow's timeout.
                    let max_polls = (radian_host::oauth_chatgpt::CALLBACK_TIMEOUT.as_secs()
                        / start.interval.as_secs().max(1))
                    .max(1) as u32;
                    let outcome = radian_host::oauth_poll_device_until_done(
                        &endpoints,
                        &start,
                        start.interval,
                        max_polls,
                    )
                    .await;
                    let result = store_oauth_outcome(outcome, &secrets);
                    let _ = tx.send(UiEvent::Command(AppInput::LoginSaved {
                        name: radian_host::CHATGPT_PROVIDER_ID.to_owned(),
                        result,
                    }));
                });
                Ok(())
            }
            // `/logout` after the `y`: remove the entry, everything else byte-for-byte (D3).
            Effect::Logout { provider } => {
                let result = radian_host::AuthStore::open()
                    .and_then(|mut store| store.remove(&provider))
                    .map_err(|error| error.to_string());
                self.send(AppInput::LogoutDone { provider, result });
                Ok(())
            }
            // The clipboard write is I/O, so it lives here (ADR 0023 U3); its result is the
            // honest status-bar notice. OSC 52 has no receipt, so success says only what
            // Radian did ("via OSC 52"); `pbcopy` can fail, and says so.
            Effect::Copy { text } => {
                let notice = match self.clipboard {
                    ClipboardMode::Off => return Ok(()),
                    ClipboardMode::Osc52 => match write_osc52(&text) {
                        Ok(()) => format!("copied {} chars (via OSC 52)", text.chars().count()),
                        Err(error) => format!("clipboard write failed: {error}"),
                    },
                    ClipboardMode::PbCopy => match run_pbcopy(&text) {
                        Ok(()) => format!("copied {} chars (via pbcopy)", text.chars().count()),
                        Err(error) => format!("pbcopy failed: {error}"),
                    },
                };
                self.send(AppInput::ClipboardDone(notice));
                Ok(())
            }
        }
    }

    /// Runs one session command. A refusal the facade reports (an undeclared capability) is a
    /// notice, not a fatal loop error; an inbox rejection keeps its old path.
    fn command(&mut self, command: radian_host::SessionCommand) -> Result<(), String> {
        match self.session.command(command) {
            Ok(()) => Ok(()),
            Err(refused) if refused.capability.is_some() => {
                self.send(AppInput::CommandRefused(refused));
                Ok(())
            }
            Err(refused) => Err(refused.reason),
        }
    }

    /// Sends an effect's result back as an event, so it is handled like every other input.
    fn send(&self, input: AppInput) {
        let _ = self.tx.send(UiEvent::Command(input));
    }
}

#[allow(clippy::too_many_arguments)]
/// Enters the terminal (raw mode + the alternate screen, and mouse reporting when `mouse`):
/// the guard restores it on every exit path, and the `Terminal` owns the frame buffer. Split
/// out because `--pick` enters it before the session and a plain run enters it after.
fn enter_terminal(
    mouse: bool,
) -> Result<
    (
        radian_tui::terminal::TtyGuard,
        Terminal<CrosstermBackend<std::io::Stdout>>,
    ),
    String,
> {
    let guard =
        radian_tui::terminal::TtyGuard::enter(mouse).map_err(|e| format!("terminal: {e}"))?;
    let terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))
        .map_err(|e| format!("terminal: {e}"))?;
    Ok((guard, terminal))
}

/// What one [`ui_loop`] run is: the pre-session `--pick` phase (only the picker overlay is
/// live) or an ordinary session (ADR 0023 U4 / plan B). One loop implementation, one
/// terminal and one input thread; the phase is the only difference.
enum Phase<'a> {
    /// No session yet. Agent/runtime/approval/dirty events have no producer, so only terminal
    /// input and the two ways to leave are handled.
    Picking,
    /// A session is open; the loop runs effects through the facade.
    Session {
        workspace: &'a Path,
        runtime: &'a tokio::runtime::Runtime,
        session: &'a Arc<dyn radian_host::SessionFacade>,
        host: &'a SessionHost,
        memory: &'a Option<MemoryService>,
    },
}

/// Handles one event in the pre-session `--pick` phase. Returns whether the frame is dirty.
fn handle_picking(event: UiEvent, app: &mut App, signal: &mut Option<i32>) -> bool {
    match event {
        UiEvent::Input(InputEvent::Key(key)) => app.on_key(key).dirty,
        UiEvent::Input(InputEvent::Paste(text)) => app.update(AppInput::Paste(text)).dirty,
        UiEvent::Input(InputEvent::Resize { rows, cols }) => {
            app.update(AppInput::Resize { rows, cols }).dirty
        }
        UiEvent::Input(InputEvent::MouseScroll(direction)) => app.mouse_scroll(direction).dirty,
        // Mouse presses during `--pick` reach the same gated path (the picker overlay blocks
        // them); there is no transcript yet, so they are no-ops at worst.
        UiEvent::Input(InputEvent::MouseDown { row, col }) => {
            app.update(AppInput::MouseDown { row, col }).dirty
        }
        UiEvent::Input(InputEvent::MouseDrag { row, col }) => {
            app.update(AppInput::MouseDrag { row, col }).dirty
        }
        UiEvent::Input(InputEvent::MouseUp { row, col }) => {
            app.update(AppInput::MouseUp { row, col }).dirty
        }
        UiEvent::Signal(value) => {
            // Remembered so the caller can exit like the session phase (128 + signal and the
            // reason line); nothing has been created.
            *signal = Some(value);
            app.quit = true;
            false
        }
        UiEvent::TerminalGone => {
            // Leave through the ordinary clean-exit path; nothing has been created.
            app.quit = true;
            false
        }
        // No session exists: these have no producer and are ignored if one somehow arrives.
        _ => false,
    }
}

/// The opencode-go usage refresh period (ADR 0027 C4): `None` (off) for `0`, the default for
/// an unset value, and the clamped configured value otherwise.
fn usage_period(configured: Option<u64>) -> Option<std::time::Duration> {
    match configured {
        Some(0) => None,
        Some(seconds) => Some(radian_host::clamp_opencode_go_period(seconds)),
        None => Some(radian_host::OPENCODE_GO_DEFAULT_PERIOD),
    }
}

/// Whether the `chatgpt` usage gate (ADR 0026) should be active for
/// `provider`: only while the current route's provider is exactly `chatgpt`, mirroring
/// opencode-go's own (inline) `provider == "opencode-go"` gate. Pulled into its own function
/// — unlike opencode-go's — so a mutation to it (polling while routed elsewhere) has a direct
/// unit test rather than needing a full TUI run to catch.
fn chatgpt_usage_active(provider: &str) -> bool {
    provider == radian_host::CHATGPT_PROVIDER_ID
}

#[cfg(test)]
mod chatgpt_usage_gate_tests {
    use super::chatgpt_usage_active;

    #[test]
    fn the_gate_is_active_only_for_chatgpt() {
        assert!(chatgpt_usage_active("chatgpt"));
        assert!(!chatgpt_usage_active("opencode-go"));
        assert!(!chatgpt_usage_active("anthropic"));
        assert!(!chatgpt_usage_active("openai"));
    }
}

/// One step of [`run_effect_rounds`]: run one effect, or take the result events the last
/// batch produced.
enum RoundStep<'a, T> {
    Execute(T),
    Drain(&'a mut Vec<T>, usize),
}

/// Runs at most [`MAX_EFFECT_ROUNDS`] effect rounds (ADR 0023 U2's draw fairness). `step` runs
/// one effect or takes the result events the last batch produced (returning whether it handled
/// any). Whatever is left in `effects` stays there for the next turn — never dropped, order
/// kept; the loop draws a frame and continues. A genuine infinite chain keeps running and warns
/// once (see [`MAX_CARRIED_FRAMES`]), it never exits. Returns the rounds run.
fn run_effect_rounds<T>(
    effects: &mut Vec<T>,
    mut step: impl FnMut(RoundStep<'_, T>) -> Result<bool, String>,
) -> Result<usize, String> {
    let mut rounds = 0;
    while !effects.is_empty() && rounds < MAX_EFFECT_ROUNDS {
        rounds += 1;
        let batch = std::mem::take(effects);
        let produced = batch.len();
        for effect in batch {
            step(RoundStep::Execute(effect))?;
        }
        if !step(RoundStep::Drain(effects, produced))? {
            break;
        }
    }
    Ok(rounds)
}

#[allow(clippy::too_many_arguments)]
fn ui_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    rx: &Receiver<UiEvent>,
    tx: &Sender<UiEvent>,
    phase: Phase<'_>,
    startup_route: (String, String),
    route_price: RoutePrice,
    config_providers: &std::collections::BTreeMap<String, radian_host::ProviderDecl>,
    // The configured theme name (`tui.theme`), for `/theme`'s current-value marker.
    current_theme: Option<String>,
    thinking_max_tokens: u32,
    bash_sandbox: Option<radian_sandbox::Sandbox>,
    // The opencode-go usage gate (ADR 0027), flipped on a `/model` switch.
    usage_gate: Option<tokio::sync::watch::Sender<bool>>,
    // The chatgpt usage gate (ADR 0026), same mechanism.
    chatgpt_usage_gate: Option<tokio::sync::watch::Sender<bool>>,
    // The herdr reporter (ADR 0034), when running inside a herdr pane.
    mut herdr: Option<HerdrReporter>,
) -> Result<Option<i32>, String> {
    let mut picking_signal: Option<i32> = None;
    let mut ctx = match phase {
        Phase::Picking => None,
        Phase::Session {
            workspace,
            runtime,
            session,
            host,
            memory,
        } => Some(LoopContext {
            startup_route: startup_route.clone(),
            route_price: Arc::clone(&route_price),
            config_providers: config_providers.clone(),
            current_thinking: app.session.thinking_level.clone().unwrap_or_default(),
            current_theme,
            thinking_max_tokens,
            workspace,
            runtime,
            session,
            memory,
            host,
            tx,
            queue: session.pending(),
            signal: None,
            clipboard: app.clipboard,
            bash_sandbox: bash_sandbox.clone(),
            bash_cancel: None,
            usage_gate,
            chatgpt_usage_gate,
            herdr: herdr.take(),
        }),
    };
    // The first frame: the startup notices and the resumed history are already in the app, and
    // zero-redraw means nothing else would draw one.
    mark("first-frame-start");
    let drawn = draw_frame(terminal, app)?;
    mark("first-frame-done");
    if !drawn {
        return Ok(None);
    }
    radian_tui::terminal::set_cursor_visible(app.input_editable());
    // The last spinner frame drawn (ADR 0027): the idle tick redraws only when the
    // frame changes, so the pulse (500 ms) does not draw on every 250 ms wake.
    let mut last_spinner: Option<radian_tui::spinner::Frame> = None;
    // An effect batch the draw-fairness cap did not finish: declared outside the loop so the
    // rest survives to the next turn (never dropped, order kept).
    let mut effects: Vec<Effect> = Vec::new();
    let mut carried_frames: u64 = 0;
    // The pre-session phase ends when the picker resolves; otherwise only `quit` leaves.
    while !app.quit && !app.picking_resolved() {
        let mut dirty = false;
        // A carried batch runs first, without a blocking wait: the effects must not wait for a
        // new event. Otherwise block until there is something to do. No display in this build
        // changes with time (the
        // status bar, the notes, the find box and the budget line are static text), so there is
        // no `recv_timeout` here: an idle app is not redrawn at all (ADR 0023 U2). A clock-like
        // display would take the timeout branch at this line. The one exception is a live
        // selection drag held at the transcript's top or bottom edge: the auto-scroll is
        // time-driven, so the loop takes a bounded timeout **only while the drag is live**,
        // and goes straight back to blocking `recv` when it ends. The armed Ctrl-C quit
        // window is the other time-driven display (ADR 0023 U2): while it is armed the loop
        // wakes at its deadline to drop the hint.
        if effects.is_empty() {
            let now = std::time::Instant::now();
            let quit_remaining = app
                .quit_pending
                .map(|deadline| deadline.saturating_duration_since(now));
            // A `/fork` waiting for the turn boundary has its own deadline (FORK_COMMIT_WAIT).
            let fork_remaining = app
                .session
                .pending_fork
                .map(|deadline| deadline.saturating_duration_since(now));
            let wait_remaining = match (quit_remaining, fork_remaining) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            };
            let first = if app.is_selecting() || wait_remaining.is_some() {
                // The nearer of the deadlines; `AUTO_SCROLL_TICK` is the drag's own tick.
                let timeout = match (app.is_selecting(), wait_remaining) {
                    (true, Some(wait)) => wait.min(AUTO_SCROLL_TICK),
                    (true, None) => AUTO_SCROLL_TICK,
                    (false, Some(wait)) => wait.max(std::time::Duration::ZERO),
                    (false, None) => unreachable!("the branch requires selection or a deadline"),
                }
                // Also wake for the hang-up check, so a drag or a deadline never postpones it
                // for longer than one tick.
                .min(TTY_CHECK_TICK);
                match rx.recv_timeout(timeout) {
                    Ok(event) => Some(event),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        // Guard 2: the input thread may be stuck inside crossterm's reader; a
                        // non-consuming `poll(2)` on stdin still reports the hang-up.
                        if terminal_is_gone() {
                            app.quit = true;
                            break;
                        }
                        // A `/fork` waiting for the journaled turn boundary: give up at its
                        // deadline with a notice, never an empty child session.
                        if app
                            .session
                            .pending_fork
                            .is_some_and(|d| std::time::Instant::now() >= d)
                            && app.update(AppInput::ForkDeadline).dirty
                            && !draw_frame(terminal, app)?
                        {
                            app.quit = true;
                            break;
                        }
                        // The quit window closed: disarm and redraw, so the hint disappears. A
                        // drag at an edge (or both) still ticks its scroll on the same wake.
                        if app
                            .quit_pending
                            .is_some_and(|d| std::time::Instant::now() >= d)
                            && app.update(AppInput::QuitDeadline).dirty
                            && !draw_frame(terminal, app)?
                        {
                            app.quit = true;
                            break;
                        }
                        if app.is_selecting()
                            && app.mouse_autoscroll()
                            && !draw_frame(terminal, app)?
                        {
                            app.quit = true;
                            break;
                        }
                        if spinner_tick(app, &mut last_spinner).is_some()
                            && !draw_frame(terminal, app)?
                        {
                            app.quit = true;
                            break;
                        }
                        continue;
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => None,
                }
            } else {
                // A bounded wait, not a blocking one: guard 2's hang-up check runs every tick even
                // while the app is idle. A timeout with the tty alive loops without drawing. The
                // spinner shortens it to its own frame interval while one is visible (ADR 0027
                // revision 13), which is what makes the breath turn at its step.
                match rx.recv_timeout(wait_for_an_event(app)) {
                    Ok(event) => Some(event),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        if terminal_is_gone() {
                            app.quit = true;
                            break;
                        }
                        if spinner_tick(app, &mut last_spinner).is_some()
                            && !draw_frame(terminal, app)?
                        {
                            app.quit = true;
                            break;
                        }
                        continue;
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => None,
                }
            };
            let Some(first) = first else {
                // Every sender is gone: nothing left to drive the UI.
                break;
            };
            let mut facts = 1;
            dirty |= match ctx.as_mut() {
                Some(ctx) => ctx.handle(first, app, &mut effects),
                None => handle_picking(first, app, &mut picking_signal),
            };
            // At most `MAX_FACTS_PER_TURN` facts before a frame (draw fairness).
            while facts < MAX_FACTS_PER_TURN {
                match rx.try_recv() {
                    Ok(event) => {
                        dirty |= match ctx.as_mut() {
                            Some(ctx) => ctx.handle(event, app, &mut effects),
                            None => handle_picking(event, app, &mut picking_signal),
                        };
                        facts += 1;
                    }
                    Err(_) => break,
                }
            }
        } else {
            // A carried batch draws a frame before the rest runs: the cap is about drawing
            // fairly, not about failing.
            dirty = true;
        }
        // Run the effects (session phase only: the picker produces none). Their results are
        // events, so they are handled by the same code; a result can ask for one more effect
        // (`@` expansion → send), so this repeats until the cap hands the rest to the next
        // turn.
        if let Some(ctx) = ctx.as_mut() {
            run_effect_rounds(&mut effects, |step| match step {
                RoundStep::Execute(effect) => ctx.execute(effect).map(|()| true),
                RoundStep::Drain(effects, produced) => {
                    // Each effect produces at most one result event; take that many (plus
                    // whatever else was already queued behind them).
                    for _ in 0..produced {
                        match rx.try_recv() {
                            Ok(event) => dirty |= ctx.handle(event, app, effects),
                            Err(_) => return Ok(false),
                        }
                    }
                    Ok(true)
                }
            })?;
            if !effects.is_empty() {
                // The cap was hit: the rest carries to the next turn (draw fairness). A chain
                // that never clears is a bug, but the TUI keeps running and says so once.
                carried_frames += 1;
                mark("effect-carry");
                if carried_frames == MAX_CARRIED_FRAMES {
                    app.push_notice("the effect worklist is not settling; continuing anyway");
                }
                dirty = true;
            } else {
                carried_frames = 0;
            }
        }
        if let Some(ctx) = ctx.as_mut()
            && let Some(herdr) = ctx.herdr.as_mut()
        {
            herdr.maybe_report(app);
        }
        if dirty {
            if !draw_frame(terminal, app)? {
                // The terminal went away between the input poll and the frame: leave cleanly.
                app.quit = true;
                break;
            }
            last_spinner = Some(app.spinner_frame());
            // The cursor is visible only while the input box is editable.
            radian_tui::terminal::set_cursor_visible(app.input_editable());
        }
    }
    // ADR 0034 H5: a normal exit releases the pane (500 ms). A panic or a kill does not: herdr
    // clears the pane itself once the shell prompt returns.
    if let Some(ctx) = ctx.as_ref()
        && let Some(herdr) = ctx.herdr.as_ref()
    {
        herdr.release();
    }
    Ok(ctx.and_then(|ctx| ctx.signal).or(picking_signal))
}

/// The record ids a `/memory` command can act on, as completion candidates. A read failure is
/// an empty list, never a turn failure (ADR 0021 invariant 2).
/// The `/model` completion candidates (ADR 0026 D5): the logged-in providers before the
/// slash, or one provider's models after it — the descriptor's built-in list plus the cached
/// `/v1/models` list. Reads local files only; the background refresh (D0) writes that cache.
fn model_route_candidates(
    current_provider: Option<&str>,
    config_providers: &std::collections::BTreeMap<String, radian_host::ProviderDecl>,
) -> Vec<radian_tui::command::Candidate> {
    // Every provider a switch could name (ADR 0026 D5/D10): the built-in descriptors, the
    // config's `[providers.<name>]` declarations, and the usable `auth.json` entries. Each is
    // resolved with `resolve_provider` so the list matches what a switch would accept, then its
    // models are flattened into one list (ADR 0035): the label is the model, the
    // description the provider.
    let auth_entries = radian_host::auth::list().unwrap_or_default();
    let mut names: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for descriptor in radian_host::descriptors() {
        if seen.insert(descriptor.id.to_owned()) {
            names.push(descriptor.id.to_owned());
        }
    }
    for name in config_providers.keys() {
        if seen.insert(name.clone()) {
            names.push(name.clone());
        }
    }
    for entry in &auth_entries {
        if entry.usable && seen.insert(entry.provider_id.clone()) {
            names.push(entry.provider_id.clone());
        }
    }
    let mut candidates = Vec::new();
    for name in &names {
        let auth_env = auth_entries
            .iter()
            .find(|entry| &entry.provider_id == name)
            .and_then(|_| radian_host::AuthStore::open().ok())
            .and_then(|store| store.entry_env(name));
        if radian_host::resolve_provider(name, config_providers, auth_env.as_ref()).is_err() {
            continue;
        }
        for model in provider_models(name) {
            candidates.push(radian_tui::command::Candidate {
                insert: format!("{name}/{model}"),
                description: name.clone(),
                current: false,
            });
        }
    }
    sort_model_candidates(&mut candidates, current_provider);
    candidates
}

/// The flat `/model` list's order (ADR 0035): the current provider's models first,
/// then by provider and model name.
fn sort_model_candidates(
    candidates: &mut [radian_tui::command::Candidate],
    current_provider: Option<&str>,
) {
    let is_current = |candidate: &radian_tui::command::Candidate| {
        current_provider.is_some_and(|provider| {
            candidate
                .insert
                .strip_prefix(provider)
                .is_some_and(|rest| rest.starts_with('/'))
        })
    };
    candidates.sort_by(|left, right| {
        is_current(right)
            .cmp(&is_current(left))
            .then_with(|| left.description.cmp(&right.description))
            .then_with(|| left.insert.cmp(&right.insert))
    });
}

/// One provider's model ids: the built-in descriptor's list, then the cached `/v1/models` ids
/// (D0), merged by id.
fn provider_models(provider: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    if let Some(descriptor) = radian_host::descriptor(provider) {
        for model in descriptor.models {
            if seen.insert(model.id.to_owned()) {
                out.push(model.id.to_owned());
            }
        }
    }
    if let Some(key) = radian_host::auth::resolve_key(provider)
        .ok()
        .flatten()
        .map(|resolved| resolved.key)
        && let Some(cache) = radian_host::read_provider_models(provider, &key)
    {
        for id in cache.models {
            if seen.insert(id.clone()) {
                out.push(id);
            }
        }
    }
    out
}

/// The [`ProviderRef`] a `/thinking` clamp/completion resolves against: the route's provider
/// name through the config's declarations and the auth entry's env (ADR 0026 D10). A name that
/// does not resolve falls back to the generic descriptor, whose map sends nothing.
fn provider_ref_for(
    name: &str,
    config_providers: &std::collections::BTreeMap<String, radian_host::ProviderDecl>,
) -> radian_host::ProviderRef {
    let auth_env = radian_host::AuthStore::open()
        .ok()
        .and_then(|store| store.entry_env(name));
    radian_host::resolve_provider(name, config_providers, auth_env.as_ref())
        .map(|(provider, _warning)| provider)
        .unwrap_or_else(|_| {
            radian_host::ProviderRef::BuiltIn(
                radian_host::descriptor(radian_host::GENERIC_ID).expect("the generic descriptor"),
            )
        })
}

fn memory_record_candidates(
    memory: &Option<MemoryService>,
    _command: &str,
) -> Vec<radian_tui::command::Candidate> {
    let Some(service) = memory else {
        return Vec::new();
    };
    let Ok(records) = service.list(radian_host::memory::ScopeSelection::Default, false) else {
        return Vec::new();
    };
    records
        .into_iter()
        .map(|record| radian_tui::command::Candidate {
            insert: record.id.as_str().to_owned(),
            description: format!(
                "{} {}",
                record.kind.as_str(),
                record_snippet(&record.content)
            ),
            current: false,
        })
        .collect()
}

/// One line of a record's content, cut for a single completion row.
fn record_snippet(content: &str) -> String {
    let first = content.lines().next().unwrap_or_default();
    let mut text: String = first.chars().take(40).collect();
    if first.chars().count() > 40 {
        text.push('…');
    }
    text
}

/// The host facts `/sessions` and `/export` need: where sessions live, which workspace this
/// process belongs to, the current session id and the values to redact.
struct SessionHost {
    dir: PathBuf,
    workspace: String,
    session_id: String,
    /// Live redaction list (shared with the panic hook); snapshot it for a renderer.
    secrets: radian_tui::terminal::SharedSecrets,
    /// The parent's writer, so `/fork` records the lineage through the same single-writer lock
    /// instead of opening a second one (ADR 0022 §1).
    journal: Arc<SessionJournal>,
    /// The child's `SessionCreated.config_snapshot` (a model-route reference).
    config_snapshot: serde_json::Value,
    /// `[tui.colors]` overrides, so a `/theme` switch keeps them (ADR 0029).
    theme_colors: Vec<(String, radian_host::config::TuiColorOverride)>,
    /// Where `/theme` writes `tui.theme` back: the file this process read, or the path it
    /// would read (ADR 0008 revision). `None` when there is no config location at all.
    theme_write_path: Option<PathBuf>,
    /// The reader's read-only verdict (a newer `schema_version`): `/theme` applies in the
    /// session but never writes.
    config_read_only: bool,
}

/// Runs one `/sessions`, `/export` or `/fork` request and returns the notices to show. Pure
/// from `App`'s point of view: the app turns these lines into entries (ADR 0023 U3).
fn handle_session_request(
    host: &SessionHost,
    runtime: &tokio::runtime::Runtime,
    request: radian_tui::command::SessionOutcome,
) -> Vec<String> {
    match request {
        radian_tui::command::SessionOutcome::Export(request) => {
            match export_current(host, &request) {
                Ok(path) => vec![format!("exported to {path}")],
                Err(message) => vec![message],
            }
        }
        radian_tui::command::SessionOutcome::Fork => {
            // ADR 0022 §1: `update` only asks for a fork when the session is idle, so there is
            // no state check here.
            let child_id = radian_host::generate_session_id();
            let child_path = host.dir.join(format!("{child_id}.jsonl"));
            let request = radian_session::ForkRequest {
                parent_path: host.journal.path().to_path_buf(),
                parent_id: host.session_id.clone(),
                seq: None,
                child_path,
                child_id: child_id.clone(),
                workspace: host.workspace.clone(),
                config_snapshot: host.config_snapshot.clone(),
            };
            match runtime.block_on(radian_session::fork_with_journal(request, &host.journal)) {
                Ok(outcome) => vec![format!(
                    "forked at seq {} into {}; open it with `radian --resume {}`                      (this window keeps the parent)",
                    outcome.seq, outcome.child_id, outcome.child_id
                )],
                Err(error) => vec![format!("fork failed: {error}")],
            }
        }
    }
}

/// Loads the data the picker overlay shows: the workspace's session rows and its index. A
/// missing session directory is one notice instead of an empty overlay (dogfooding D2 style).
fn load_picker(host: &SessionHost, query: Option<String>) -> AppInput {
    match radian_host::sessions::open_index(&host.dir) {
        Ok(Some(index)) => match radian_host::sessions::list(&index, &host.workspace) {
            Ok(rows) => AppInput::PickerOpened(PickerData {
                rows,
                index: Some(index),
                workspace: host.workspace.clone(),
                query,
                secrets: radian_tui::terminal::shared_secrets(&host.secrets),
            }),
            Err(message) => AppInput::SessionDone(vec![message]),
        },
        Ok(None) => AppInput::SessionDone(vec!["no sessions for this workspace yet".to_owned()]),
        Err(message) => AppInput::SessionDone(vec![message]),
    }
}

/// `/export`: render the current session with the shared host layer, check overwrite, and
/// write atomically. Returns the absolute path for the notice.
fn export_current(
    host: &SessionHost,
    request: &radian_tui::command::ExportRequest,
) -> Result<String, String> {
    let extension = request.format.extension();
    let path = match &request.path {
        Some(path) => {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                path
            } else {
                PathBuf::from(&host.workspace).join(path)
            }
        }
        None => {
            PathBuf::from(&host.workspace).join(format!("radian-{}.{extension}", host.session_id))
        }
    };
    if path.exists() && !request.force {
        return Err(format!(
            "{} already exists; `/export {} {} --force` overwrites it",
            path.display(),
            match request.format {
                radian_host::Format::Markdown => "md",
                radian_host::Format::Json => "json",
            },
            path.display()
        ));
    }
    let settings = radian_host::ExportSettings {
        include_reasoning: false,
        include_provider_payload: false,
        secrets: radian_tui::terminal::shared_secrets(&host.secrets),
    };
    let rendered = radian_host::sessions::export(
        &host.dir,
        &host.workspace,
        &host.session_id,
        request.format,
        &settings,
    )?;
    write_atomic(&path, &rendered)?;
    Ok(std::fs::canonicalize(&path)
        .unwrap_or(path)
        .display()
        .to_string())
}

/// Writes `contents` to `path` atomically: a sibling temp file, then a rename. 0644 (the file
/// is for the user; it is not a session file).
fn write_atomic(path: &Path, contents: &str) -> Result<(), String> {
    static EXPORT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    let seq = EXPORT_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let tmp = parent.join(format!(".radian-export-{}-{seq}.tmp", std::process::id()));
    let write = || -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o644);
        }
        let mut file = options.open(&tmp)?;
        use std::io::Write;
        file.write_all(contents.as_bytes())?;
        file.sync_all()
    };
    if let Err(error) = write() {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("cannot write {}: {error}", path.display()));
    }
    if let Err(error) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("cannot replace {}: {error}", path.display()));
    }
    Ok(())
}

/// Where session files live: `--session-dir`, then `RADIAN_SESSION_DIR`, then the user's data
/// directory (ADR 0020: `~/.radian/sessions`). The rule lives in `radian-host` so a session
/// written by one host is resumable by the other (ADR 0032 slice 5).
fn session_dir(cli: &Cli) -> Result<PathBuf, String> {
    radian_host::session_dir(cli.session_dir.as_deref())
}

/// The workspace a session belongs to: `--workspace` when given, else the current directory,
/// through the TUI's coding root (`$HOME` becomes `$HOME/tmp`, the filesystem root is refused)
/// and then the shared `radian-host` rule. `--list` filters by the same value, so a session is
/// only listed in the workspace it was created in, and a bad `--workspace` fails as a workspace
/// error (dogfooding D1).
fn session_workspace(cli: &Cli) -> Result<String, String> {
    radian_host::session_workspace(Some(&coding_root(cli)?.path))
}

/// The coding root: `--workspace` as given, else the current directory, with the TUI's two
/// exceptions — `$HOME` becomes `$HOME/tmp` and the filesystem root
/// is refused. The CLI (`radian exec`) does **not** do this: it is the unattended entry point and
/// gets no write access by default.
fn coding_root(cli: &Cli) -> Result<radian_tui::workspace::CodingRoot, String> {
    let cwd = std::env::current_dir()
        .map_err(|error| format!("cannot read the current directory: {error}"))?;
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    radian_tui::workspace::resolve_coding_root(cli.workspace.as_deref(), &cwd, home.as_deref())
}

/// `--list`: this workspace's sessions, newest activity first (the shared table, ADR 0032
/// slice 5).
fn list_sessions(cli: &Cli) -> Result<(), radian_host::ListError> {
    let dir = session_dir(cli)?;
    let workspace = session_workspace(cli)?;
    radian_host::list_sessions(&dir, &workspace)
}

/// The Phase 4 startup picker: opens the workspace index, lists the sessions, and (when there
/// are any) runs the full-screen picker. `Ok(None)` means there are no sessions yet, so the
/// caller starts a new one with a notice instead of flashing an empty list.
fn prepare_picker(
    dir: &Path,
    workspace: &str,
) -> Result<Option<radian_tui::picker::Picker>, String> {
    let index = radian_host::sessions::open_index(dir)?;
    let rows = match &index {
        Some(index) => radian_host::sessions::list(index, workspace)?,
        None => Vec::new(),
    };
    if rows.is_empty() {
        return Ok(None);
    }
    Ok(Some(radian_tui::picker::Picker::new(
        rows, index, workspace,
    )))
}

/// Writes every decision to the session journal as a `PermissionResolved` (ADR 0006 §5.9).
/// The append is spawned: `AuditSink::record` is synchronous and runs inside the hook, so it
/// must not block. The record is audit only (`critical = false`) and is never replayed.
/// A policy switch (ADR 0026 D9) is journaled the same way, as `ApprovalPolicyChanged`.
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

    fn record_policy_change(&self, policy: radian_permissions::ApprovalPolicy) {
        let event = radian_permissions::policy_change_event(policy);
        let journal = Arc::clone(&self.journal);
        self.runtime.spawn(async move {
            let _ = journal.append_runtime_event(event).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR 0023 U2's draw fairness: a chain longer than `MAX_EFFECT_ROUNDS` carries the rest to
    /// the next turn — never dropped, order kept — instead of failing the whole TUI. This is
    /// the `the effect worklist did not settle` bug: under load the loop's drain consumes
    /// unrelated events whose handlers produce more effects, so a real chain can exceed the cap.
    #[test]
    fn effect_rounds_carry_the_rest_in_order() {
        let mut pending: Vec<u32> = vec![19];
        let mut executed: Vec<u32> = Vec::new();
        let mut next: Vec<u32> = Vec::new();
        let mut calls = 0;
        while !pending.is_empty() {
            calls += 1;
            assert!(calls < 100, "the carry must terminate");
            let rounds = run_effect_rounds(&mut pending, |step| match step {
                RoundStep::Execute(n) => {
                    executed.push(n);
                    if n > 0 {
                        next.push(n - 1);
                    }
                    Ok(true)
                }
                RoundStep::Drain(pending, produced) => {
                    // Each executed effect produces one result event; handling it produces the
                    // next effect.
                    for _ in 0..produced {
                        if next.is_empty() {
                            return Ok(false);
                        }
                        pending.push(next.remove(0));
                    }
                    Ok(true)
                }
            })
            .unwrap();
            assert!(rounds <= MAX_EFFECT_ROUNDS, "each call is capped");
        }
        assert_eq!(
            executed,
            (0..20).rev().collect::<Vec<_>>(),
            "every effect ran, in order"
        );
    }

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

    /// ADR 0035: the current provider's models come first in the flat `/model`
    /// list, then the rest by provider and model name.
    #[test]
    fn the_flat_model_list_puts_the_current_provider_first() {
        let candidate = |insert: &str, provider: &str| radian_tui::command::Candidate {
            insert: insert.to_owned(),
            description: provider.to_owned(),
            current: false,
        };
        let mut candidates = vec![
            candidate("openai/gpt-5", "openai"),
            candidate("opencode-go/zeta", "opencode-go"),
            candidate("anthropic/claude-4", "anthropic"),
            candidate("opencode-go/alpha", "opencode-go"),
        ];
        sort_model_candidates(&mut candidates, Some("opencode-go"));
        let order: Vec<&str> = candidates.iter().map(|c| c.insert.as_str()).collect();
        assert_eq!(
            order,
            [
                "opencode-go/alpha",
                "opencode-go/zeta",
                "anthropic/claude-4",
                "openai/gpt-5"
            ],
            "the current provider's models first, then by provider and model"
        );
        // Without a current provider, only the name order applies.
        sort_model_candidates(&mut candidates, None);
        let order: Vec<&str> = candidates.iter().map(|c| c.insert.as_str()).collect();
        assert_eq!(
            order,
            [
                "anthropic/claude-4",
                "openai/gpt-5",
                "opencode-go/alpha",
                "opencode-go/zeta"
            ]
        );
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
