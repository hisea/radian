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

use clap::Parser;
use radian_core::{AgentEvent, ContextBudget, EventSink, TurnOptions};
use radian_host::memory::MemoryService;
use radian_host::{
    DEFAULT_MAX_TOKENS, ModelArgs, SessionFacade, Thinking, build_model, check_context_window_flag,
    check_provider_flags, is_local_base_url, load_mcp_config, parse_headers, validate_session_id,
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

mod arc;
mod attach;
mod herdr;
mod loop_context;
mod models;
mod run_finish;
mod run_open;
mod run_ready;
mod run_resolve;
mod run_startup;
mod session;
mod tty;
mod ui;

use arc::run_arc as run_arc_tui;
use attach::run_attach;
use herdr::{HerdrReporter, herdr_reporter};
use loop_context::LoopContext;
use models::{
    mark_current_route, memory_record_candidates, model_refresh_targets, model_route_candidates,
    provider_ref_for, refresh_provider_models,
};
use session::{
    JournalAudit, SessionHost, coding_root, handle_session_request, list_sessions, load_picker,
    prepare_picker, session_dir,
};
use tty::panic_in_driver;

use tty::*;
use ui::*;

#[derive(Parser, Debug)]
#[command(
    name = "radian",
    version,
    about = "Full-screen TUI for the radian agent runtime",
    // ADR 0032 A1: the joined surface is longer than either binary's, so the commands and
    // the flags are grouped.
    subcommand_help_heading = "Commands"
)]
struct Cli {
    // The TUI's own setup flags come first: the default mode is the full-screen TUI, and the
    // `--help` groups read in the order (TUI, session, model, mcp,
    // permissions).
    /// The configuration file. Overrides `RADIAN_CONFIG` and
    /// `~/.radian/config/config.toml`. Read-only: radian never writes it.
    #[arg(help_heading = "TUI", long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Expose the built-in file tools rooted at this directory (read/write/patch,
    /// glob/grep). The `shell` and `job` tools are exposed as well: under `workspace` they run
    /// confined, under `full` (the default) unsandboxed. Omitted, the current directory is
    /// the workspace.
    #[arg(help_heading = "TUI", long, short = 'w', value_name = "DIR")]
    workspace: Option<PathBuf>,

    #[arg(help_heading = "TUI", long)]
    /// System prompt text.
    system: Option<String>,

    /// The model name sent to the API — a bare name, never `<provider>/<model>`; choose the
    /// provider with `--provider`. Required unless the config file's `model.model` provides it.
    #[arg(help_heading = "Model", long, short = 'm')]
    model: Option<String>,

    /// Provider wire protocol: `openai-compatible` (the default) or `anthropic`.
    #[arg(help_heading = "Model", long, value_name = "PROVIDER")]
    provider: Option<String>,

    /// Everything before the provider's path (`/chat/completions` for openai-compatible,
    /// `/v1/messages` for anthropic). Defaults to the provider's public endpoint.
    #[arg(help_heading = "Model", long, value_name = "URL")]
    base_url: Option<String>,

    /// `max_tokens` for anthropic (the Messages API requires it). Refused for
    /// openai-compatible. Defaults to 16384.
    #[arg(help_heading = "Model", long, value_name = "N")]
    max_tokens: Option<u32>,

    /// Thinking level: one of `off|minimal|low|medium|high|xhigh|max` (default `medium`),
    /// clamped to what the route supports. The legacy `adaptive` and `enabled:<budget>`
    /// spellings still work with a warning.
    #[arg(help_heading = "Model", long, value_name = "MODE")]
    thinking: Option<String>,

    /// Extra request header, repeatable: `--header NAME=VALUE`.
    #[arg(help_heading = "Model", long = "header", value_name = "NAME=VALUE")]
    header: Vec<String>,

    /// Where session files live. Defaults to `RADIAN_SESSION_DIR`, then `~/.radian/sessions`
    /// (`$RADIAN_HOME/sessions` when that is set). The same rule as `radian exec`.
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

    /// Show a full-screen picker for this workspace's sessions before opening one:
    /// Up/Down select, type to filter, Tab full-text search, Enter resume, n new, Esc quit.
    /// Conflicts with `--resume`/`--session-id`; it does not create a session file.
    #[arg(help_heading = "Session", long, conflicts_with_all = ["resume", "session_id"])]
    pick: bool,

    /// A JSON file listing MCP stdio servers to start (`{"servers": [...]}`), the same format
    /// `radian exec` reads. Under `workspace` their tools ask before running and the
    /// servers themselves are **not** sandboxed.
    #[arg(help_heading = "MCP", long, value_name = "FILE")]
    mcp_config: Option<String>,

    /// The permission tier: `workspace` (sandboxed: writes confined to the workspace,
    /// `shell` sandboxed, network on), or `full` (the default:
    /// unsandboxed, no built-in prompts). A project's `.radian/config.toml` may narrow it,
    /// and the config file's `permissions.profile` is tried in between.
    #[arg(help_heading = "Permissions", long, value_name = "TIER")]
    profile: Option<String>,

    /// How `ask` is resolved: `on-ask` asks when a rule says so,
    /// `always` asks for every call a rule does not deny, `never` turns `ask` into a denial,
    /// `auto` answers every `ask` with "allow". Omit to fall back to the
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
    /// "git status"`). Only a *simple* command matches: any shell metacharacter keeps that
    /// segment at `ask`, so a compound command is allowed only when every segment is allowed.
    /// Write these at the subcommand level, never at a bare program name.
    #[arg(
        help_heading = "Permissions",
        long = "allow-shell-command",
        value_name = "PREFIX"
    )]
    allow_shell_command: Vec<String>,

    /// The model's context window in tokens. Omitted, the models.dev catalog is
    /// consulted for `(provider, model)`; an explicit value always wins. With
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
    /// unchanged.
    #[arg(help_heading = "Model", long)]
    no_auto_compact: bool,

    /// Trust this workspace's project resources for **this process only** (never written to
    /// `trust.json`).
    #[arg(help_heading = "Permissions", long, conflicts_with = "no_trust")]
    trust: bool,

    /// Do not trust this workspace's project resources for this process only.
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

    /// A non-TUI mode: `radian exec` runs one turn or an interactive line session, and the
    /// management commands (`radian memory`/`login`/`config`) run the same library dispatch.
    /// Absent: the full-screen TUI.
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

/// The subcommands that are not the TUI. `exec` runs one turn or an interactive line session
/// through the `radian-exec` library so the two entries cannot drift; the management commands
/// follow in the same shape.
#[derive(clap::Subcommand, Debug)]
enum RadianCommand {
    /// Run one turn or an interactive line session.
    /// `radian exec -p "…"` prints only the model's text; `radian exec` is the line REPL.
    ///
    /// The `agent` parser owns this mode's flags, help and usage errors (`radian exec --help`
    /// shows them, and a bad flag exits 2 through clap, exactly as `agent` does), so clap's
    /// own help for this variant is off and everything after `exec` is passed through.
    #[command(disable_help_flag = true, disable_version_flag = true)]
    Exec {
        /// Everything `radian exec` accepts, passed through unchanged.
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "ARGS"
        )]
        args: Vec<String>,
    },

    /// Serve one session over the Agent Client Protocol (stdio).
    /// `radian acp` accepts `radian exec`'s conversation flags, without `-p`, `--output` and
    /// `--yes`; the `acp` parser owns its flags and help, so everything after `acp` is passed
    /// through.
    #[command(disable_help_flag = true, disable_version_flag = true)]
    Acp {
        /// Everything `radian acp` accepts, passed through unchanged.
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "ARGS"
        )]
        args: Vec<String>,
    },

    /// Store an API key for a provider. The same code `radian exec login` runs.
    Login {
        /// A built-in provider id, or `custom` to name a new endpoint. Omitted: a numbered
        /// list is shown.
        provider: Option<String>,
    },
    /// Remove a stored credential (asks first). The same code `radian exec logout` runs.
    Logout {
        /// The provider id the key is stored under.
        provider: String,
        /// The key's name: remove that one without asking.
        name: Option<String>,
    },
    /// Manage durable memory: the same grammar as `radian exec memory` and the
    /// TUI's `/memory`.
    Memory {
        /// The workspace project memory is resolved against, like `radian exec --workspace`.
        #[arg(long, value_name = "DIR")]
        workspace: Option<PathBuf>,
        #[command(subcommand)]
        command: radian_host::memory::commands::MemoryCommand,
    },
    /// Show or change a directory's project trust decision: the same grammar as
    /// `radian exec trust` and the TUI's `/trust`.
    Trust {
        /// The workspace the decision is about, like `radian exec --workspace`. Omitted:
        /// the current directory.
        #[arg(long, value_name = "DIR")]
        workspace: Option<PathBuf>,
        #[command(subcommand)]
        command: radian_exec::TrustCommand,
    },
    /// Configuration tools: `radian config import-pi …`, the same code
    /// `radian exec config` runs.
    Config {
        #[command(subcommand)]
        command: radian_exec::ConfigCommand,
    },

    /// Theme tools: `radian theme import <pi-theme.json>`
    /// converts a pi theme to a Radian TOML theme under `~/.radian/themes/`.
    Theme {
        #[command(subcommand)]
        command: ThemeCommand,
    },

    /// The plan and task orchestrator: set arc up, and read and write plans and tasks.
    /// A bare `radian arc` opens the read-only arc TUI; `radian arc <verb>` runs the CLI.
    Arc {
        #[command(subcommand)]
        command: Option<ArcCommand>,
    },

    /// Footer tools: `radian footer export <preset>`
    /// prints a built-in preset's TOML exactly, so a user can copy and edit it.
    Footer {
        #[command(subcommand)]
        command: FooterCommand,
    },

    /// Read the bundled user manual. No arguments prints the topic index; a topic prints
    /// that page. The pages are compiled into the binary, the same text `read` sees.
    Docs {
        /// The manual topic: a page stem like `commands`, or `commands.md`; a directory
        /// topic like `arc` opens its `index.md`. Omitted prints one line per topic with its
        /// description.
        #[arg(value_name = "TOPIC")]
        topic: Option<String>,
    },

    /// View another session read-only: its committed transcript, rendered
    /// with the same blocks a `--resume` uses. No input reaches the session, no socket is
    /// opened, and nothing is written.
    Attach {
        /// The session id: the name of its file in the session directory. An exact id, like
        /// `--resume` (no prefix matching). Omitted, this workspace's session picker opens and
        /// Enter attaches to the choice (Esc/q leaves without attaching; no session file is
        /// created either way).
        #[arg(value_name = "SESSION_ID")]
        session_id: Option<String>,
        /// Keep reading: show the committed events the session's own process appends, until
        /// `q`/Esc/Ctrl-C. Without it the view reads once and stays still.
        #[arg(long)]
        follow: bool,
        /// Where session files live. Defaults to `RADIAN_SESSION_DIR`, then `~/.radian/sessions`
        /// (`$RADIAN_HOME/sessions`), the same rule as the top-level `--session-dir`.
        #[arg(long, value_name = "DIR")]
        session_dir: Option<PathBuf>,
    },

    /// Serve a session over a local endpoint. Not available in this build: the command exits 2
    /// without serving anything.
    Serve,
    /// Serve the session's tools over MCP. Not available in this build: the command exits 2
    /// without serving anything.
    Mcp,
}

/// The `radian footer` subcommands.
#[derive(clap::Subcommand, Debug)]
enum FooterCommand {
    /// Print a built-in preset's TOML exactly (`deck`/`full`/`pi`/`classic`): using it as
    /// the config's `[tui.footer]` section gives the same footer, so it is the starting
    /// point for a user customization.
    Export {
        /// The preset to print: one of `deck`, `full`, `pi`, `classic`.
        #[arg(value_name = "PRESET")]
        preset: String,
    },
}

/// The `priority` argument's parser. It lists [`radian_arc::store::PLAN_PRIORITIES`], the one
/// list the store also checks against, as the possible values `--help` shows; it accepts any
/// text, because a value outside the set is the store's refusal (exit 3), not clap's usage
/// error (2).
#[derive(Clone)]
struct PriorityValueParser;

impl clap::builder::TypedValueParser for PriorityValueParser {
    type Value = String;

    fn parse_ref(
        &self,
        _cmd: &clap::Command,
        _arg: Option<&clap::Arg>,
        value: &std::ffi::OsStr,
    ) -> Result<Self::Value, clap::Error> {
        value.to_str().map(str::to_owned).ok_or_else(|| {
            clap::Error::raw(
                clap::error::ErrorKind::InvalidUtf8,
                "the plan priority is not valid UTF-8",
            )
        })
    }

    fn possible_values(
        &self,
    ) -> Option<Box<dyn Iterator<Item = clap::builder::PossibleValue> + '_>> {
        Some(Box::new(radian_arc::store::PLAN_PRIORITIES.iter().map(
            |priority| clap::builder::PossibleValue::new(*priority),
        )))
    }
}

/// The `radian arc` subcommands. The grammar lives here so `radian arc --help` and the
/// manual's drift test read one parser; everything the subcommands do is in `radian-arc`.
#[derive(clap::Subcommand, Debug)]
enum ArcCommand {
    /// Set arc up for this repository and check its requirements.
    Init {
        /// Track `config.toml`, `plans/` and `tasks/` in git. arc itself never commits them.
        #[arg(long)]
        track: bool,
    },

    /// Create a plan or a task. The request text or the brief is read from stdin.
    Add {
        #[command(subcommand)]
        command: ArcAddCommand,
    },

    /// List plans, each with its tasks in order. Merged history and cancelled plans are
    /// hidden unless `--all`.
    List {
        /// Only this plan's id: a 1 to 4 letter prefix, four digits, a dash and a slug.
        #[arg(long, value_name = "ID")]
        plan: Option<String>,
        /// Show every plan and every task, including finished and cancelled plans and merged
        /// tasks.
        #[arg(long)]
        all: bool,
        /// Print JSON. Without `--all` it follows the same filter: a cancelled plan is left
        /// out.
        #[arg(long)]
        json: bool,
    },

    /// Show one plan or task: every field and the body.
    Show {
        /// The plan or task id: a 1 to 4 letter prefix, four digits, a dash and a slug.
        #[arg(value_name = "ID")]
        id: String,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },

    /// Accept a plan or task file that changed outside arc.
    Adopt {
        /// The plan or task id: a 1 to 4 letter prefix, four digits, a dash and a slug.
        #[arg(value_name = "ID")]
        id: String,
    },

    /// Internal: run one attempt of one task. arc runs it, a person does not; it is hidden
    /// and has no manual page. The brief is read from stdin; the last stdout line is the
    /// `AttemptFinished` record.
    #[command(hide = true)]
    Work {
        /// The task id: the session id, the branch suffix and the worktree name.
        #[arg(long, value_name = "TASK", hide = true)]
        task: String,
        /// The attempt number, 1-based.
        #[arg(long, value_name = "N", hide = true)]
        attempt: u32,
        /// The random token the scheduling loop expects back in `AttemptFinished`.
        #[arg(long, value_name = "TOKEN", hide = true)]
        token: String,
        /// Internal: the worker's session id. arc passes `<task>-<attempt>` with
        /// `[worker] rework_session = "fresh"` and the task id with `resume`; absent, the
        /// task id. Not in the manual.
        #[arg(long, value_name = "ID", hide = true)]
        session: Option<String>,
        /// Continue the session (`--resume <session>`) instead of starting one; a `fresh`
        /// attempt never passes it.
        #[arg(long, hide = true)]
        resume: bool,
        /// Let a branch with no change past the base pass the delivery check.
        #[arg(long, hide = true)]
        allow_noop: bool,
        /// Internal: the remote to fetch the main branch from and push to. arc sets it from
        /// the repository configuration; a person does not. Not in the manual.
        #[arg(long, value_name = "NAME", hide = true)]
        remote: Option<String>,
        /// Internal: the main branch a first attempt starts from. arc sets it; a person does
        /// not. Not in the manual.
        #[arg(long, value_name = "NAME", hide = true)]
        main_branch: Option<String>,
        /// Extra `radian exec` arguments, after `--` (the model route in tests).
        #[arg(
            last = true,
            allow_hyphen_values = true,
            value_name = "ARGS",
            hide = true
        )]
        extra: Vec<String>,
        /// Internal: the configured `[worker] command`, as one JSON array with the program
        /// first. arc passes it for a command worker; unset runs `radian exec`. Not in the
        /// manual.
        #[arg(long, value_name = "JSON", hide = true)]
        command: Option<String>,
        /// Internal: the task's flow, the name the attempt's records carry. arc passes the
        /// flow the claim resolved. Not in the manual.
        #[arg(long, value_name = "NAME", hide = true)]
        flow: Option<String>,
        /// Test-only: run this shell command as the worker instead of `radian exec`.
        /// The wrapper's forwarding test starts a plain process with a child of its own.
        #[arg(long, value_name = "SHELL-COMMAND", hide = true)]
        worker_command: Option<String>,
        /// Internal: the attempt's stall limit in seconds; `0` means no limit. arc passes the
        /// task's value or `[limits] stall_timeout_secs`. Not in the manual.
        #[arg(long, value_name = "SECONDS", hide = true)]
        stall_timeout_secs: Option<u64>,
        /// Internal: the attempt's absolute wall-time limit in seconds; `0` means no limit.
        /// Not in the manual.
        #[arg(long, value_name = "SECONDS", hide = true)]
        max_wall_time_secs: Option<u64>,
        /// Internal: the grace period before SIGKILL after a signalled worker
        /// (`[limits] cancel_grace_secs`). Not in the manual.
        #[arg(long, value_name = "SECONDS", hide = true)]
        cancel_grace_secs: Option<u64>,
    },

    /// Internal: run one task's gate in the background. A scheduling pass starts it and does
    /// not wait; it is hidden and has no manual page.
    #[command(hide = true)]
    Gate {
        /// The task whose gate to run.
        #[arg(long, value_name = "TASK", hide = true)]
        task: String,
        /// The inherited lock descriptor: it keeps one gate per task at a time.
        #[arg(long, value_name = "FD", hide = true)]
        lock_fd: Option<i32>,
    },

    /// Confirm a draft plan, or a task arc or the lead proposed. A plan with no tasks is
    /// refused, and so is a second confirm; a task that is not `todo.proposed` is refused,
    /// and a confirmed task becomes `todo.ready` without changing its plan.
    Confirm {
        /// The plan id, or the id of a `todo.proposed` task.
        #[arg(value_name = "ID")]
        id: String,
        /// Run every check the confirm runs, print what it would do, and write nothing. It
        /// refuses with the same message and exit code as a real confirm.
        #[arg(long)]
        dry_run: bool,
    },

    /// Set a plan's auto level. Only a confirmed plan can use a level other than `off`.
    Auto {
        /// The plan id.
        #[arg(value_name = "PLAN")]
        plan: String,
        /// The level: `off` starts nothing, `run` starts tasks and the gate, `merge` also
        /// merges what passed.
        #[arg(
            value_name = "LEVEL",
            value_parser = clap::builder::PossibleValuesParser::new(
                radian_arc::store::AUTO_LEVELS.iter().copied()
            )
        )]
        level: String,
    },

    /// Set a plan's auto level back to `off`. Tasks that already started are not touched.
    Pause {
        /// The plan id.
        #[arg(value_name = "PLAN")]
        plan: String,
    },

    /// Set a plan's priority: `P0` is the highest, `P5` the lowest, and `P3` is what a plan
    /// without the field reads as. The next pass claims a higher priority's tasks first and
    /// never preempts a running attempt.
    Priority {
        /// The plan id, in any state.
        #[arg(value_name = "PLAN")]
        plan: String,
        /// The priority. A value outside the set is refused with exit 3.
        #[arg(value_name = "PRIORITY", value_parser = PriorityValueParser)]
        priority: String,
    },

    /// Claim a task and run one attempt of it, waiting until it ends.
    Start {
        /// The task id.
        #[arg(value_name = "TASK")]
        task: String,
    },

    /// Re-run the gate for a task's already pushed commit without running a worker. It
    /// applies to a task in `review.rework_pending` or `review.awaiting_human` whose rework
    /// reason is a verification stage; it claims a new attempt and verifies it at once.
    Reverify {
        /// The task id.
        #[arg(value_name = "TASK")]
        task: String,
    },

    /// Stop a task, or end every open task of a plan and the plan itself.
    Cancel {
        /// The task id, or the plan id.
        #[arg(value_name = "ID")]
        id: String,
    },

    /// Move a budget-paused task, or every paused task of a plan, back to `todo.ready`.
    ResetBudget {
        /// The task id, or the plan id to reset every paused task of the plan.
        #[arg(value_name = "ID")]
        id: String,
    },

    /// Approve a verified task, integrate it, and push the result to the remote main. A main
    /// that moved makes the command integrate again.
    Merge {
        /// The task id. It must be `review.awaiting_human`.
        #[arg(value_name = "TASK")]
        task: String,
    },

    /// Refuse a task in `review.awaiting_human` with a note, and send it back to rework.
    Reject {
        /// The task id. It must be `review.awaiting_human`.
        #[arg(value_name = "TASK")]
        task: String,
        /// Why the task is refused. `show` prints it as the rework reason.
        #[arg(long, value_name = "TEXT")]
        note: String,
    },

    /// Make every transition the auto levels allow now, then exit: reconcile, release due
    /// retries, start a task's gate or a worker, and drive a merge at auto level `merge`. A
    /// pass only schedules: the worker and the gate run outside it and a later pass collects
    /// them. Use `radian arc serve` to repeat the pass.
    Run {
        /// Refused: `run --watch` is now `radian arc serve`. The flag exists only so the
        /// refusal names the new command in one exact line; the manual documents it as a
        /// refused input.
        #[arg(long, hide = true)]
        watch: bool,
        /// Accepted with `--watch` only so the refusal is the exact line above instead of
        /// clap's unknown-argument error. Hidden: it is part of the refused input, not an
        /// input of `run`.
        #[arg(long, value_name = "SECONDS", requires = "watch", hide = true)]
        interval: Option<u64>,
    },

    /// Run the scheduling loop: repeat the pass (which only schedules) every `--interval`
    /// seconds until SIGINT or SIGTERM, holding `runner.lock` the whole time.
    Serve {
        /// Seconds between passes: 5 to 3600. The default is 30.
        #[arg(
            long,
            value_name = "SECONDS",
            value_parser = clap::value_parser!(u64).range(
                radian_arc::run::MIN_INTERVAL_SECS..=radian_arc::run::MAX_INTERVAL_SECS
            )
        )]
        interval: Option<u64>,
    },

    /// Serve the lead's MCP tools over standard input and output. A lead registers this
    /// command; a person does not run it.
    Mcp {
        /// Push task transitions to the lead over Claude Code's channel extension: the
        /// server declares `claude/channel`, narrows its supported protocol versions to
        /// `2025-11-25` and below, and sends `notifications/claude/channel` for the tracked
        /// transitions. Off: the server is the plain tool server and sends nothing.
        #[arg(long)]
        claude_experimental: bool,
    },
}

/// The `radian arc add` subcommands.
#[derive(clap::Subcommand, Debug)]
enum ArcAddCommand {
    /// Create a plan from the request text on stdin.
    Plan {
        /// The slug: 1 to 48 characters from `a-z`, `0-9` and `-`. arc puts the plan's
        /// configured prefix and the next number before it.
        #[arg(long, value_name = "SLUG")]
        slug: String,
        /// The plan's title.
        #[arg(long, value_name = "TITLE")]
        title: String,
    },

    /// Create a task in a draft plan from the brief on stdin.
    Task {
        /// The plan the task belongs to (its id).
        #[arg(long, value_name = "ID")]
        plan: String,
        /// The slug: 1 to 48 characters from `a-z`, `0-9` and `-`. arc puts the task's
        /// configured prefix and the next number before it.
        #[arg(long, value_name = "SLUG")]
        slug: String,
        /// The task's title.
        #[arg(long, value_name = "TITLE")]
        title: String,
        /// A task this one waits for: a task of the same plan, or a done task. Repeat it.
        #[arg(long = "after", value_name = "ID")]
        after: Vec<String>,
        /// The task may deliver no change on purpose. Without the flag the task uses its
        /// flow's [flows.<name>] allow_noop; `--allow-noop` alone means `true`.
        #[arg(long, num_args = 0..=1, default_missing_value = "true", value_name = "BOOL")]
        allow_noop: Option<bool>,
        /// An extra gate stage to run for this task. Repeat it.
        #[arg(
            long = "stage",
            value_name = "NAME",
            value_parser = clap::builder::PossibleValuesParser::new(
                radian_arc::store::config::STAGE_NAMES.iter().copied()
            )
        )]
        stages: Vec<String>,
        /// The flow whose worker runs every attempt of this task: a [flows.<name>] section
        /// of config.toml. Default: default.
        #[arg(long, value_name = "NAME")]
        flow: Option<String>,
    },
}

impl ArcCommand {
    /// The store operation, for every subcommand except the internal commands a person does
    /// not run (`work` and `gate`).
    fn into_store_command(self) -> Option<radian_arc::store::cli::Command> {
        use radian_arc::store::cli::Command as ArcRun;
        Some(match self {
            ArcCommand::Init { track } => ArcRun::Init { track },
            ArcCommand::Add {
                command: ArcAddCommand::Plan { slug, title },
            } => ArcRun::AddPlan { slug, title },
            ArcCommand::Add {
                command:
                    ArcAddCommand::Task {
                        plan,
                        slug,
                        title,
                        after,
                        allow_noop,
                        stages,
                        flow,
                    },
            } => ArcRun::AddTask {
                plan,
                slug,
                title,
                after,
                allow_noop,
                stages,
                flow,
            },
            ArcCommand::List { plan, all, json } => ArcRun::List { plan, all, json },
            ArcCommand::Show { id, json } => ArcRun::Show { id, json },
            ArcCommand::Adopt { id } => ArcRun::Adopt { id },
            ArcCommand::Confirm { id, dry_run } => ArcRun::Confirm { id, dry_run },
            ArcCommand::Auto { plan, level } => ArcRun::Auto { plan, level },
            ArcCommand::Priority { plan, priority } => ArcRun::Priority { plan, priority },
            ArcCommand::Pause { plan } => ArcRun::Pause { plan },
            ArcCommand::Start { task } => ArcRun::Start { task },
            ArcCommand::Reverify { task } => ArcRun::Reverify { task },
            ArcCommand::Cancel { id } => ArcRun::Cancel { id },
            ArcCommand::ResetBudget { id } => ArcRun::ResetBudget { id },
            ArcCommand::Merge { task } => ArcRun::Merge { task },
            ArcCommand::Reject { task, note } => ArcRun::Reject { task, note },
            ArcCommand::Run { .. } => ArcRun::Run,
            ArcCommand::Serve { interval } => ArcRun::Serve {
                interval: interval.unwrap_or(radian_arc::run::DEFAULT_INTERVAL_SECS),
            },
            ArcCommand::Mcp {
                claude_experimental,
            } => ArcRun::Mcp {
                claude_experimental,
            },
            // `work` is the trusted wrapper and `gate` is the background gate: not store
            // operations. `run_arc` dispatches both before this conversion.
            ArcCommand::Work { .. } | ArcCommand::Gate { .. } => return None,
        })
    }
}

/// Runs `radian arc`: the arguments are parsed here, the work is in `radian-arc`.
fn run_arc(command: Option<ArcCommand>, config: Option<&Path>) -> ExitCode {
    match command {
        // The wrapper is internal and has no store operation. `remote` and `main_branch`
        // default to the repository's `config.toml`, which `radian arc init` wrote; the git
        // defaults are the fallback for a repository arc is not set up in.
        Some(ArcCommand::Work {
            task,
            attempt,
            token,
            session,
            resume,
            allow_noop,
            remote,
            main_branch,
            extra,
            command,
            flow,
            worker_command,
            stall_timeout_secs,
            max_wall_time_secs,
            cancel_grace_secs,
        }) => {
            let (remote, main_branch) = work_remote_and_main(remote, main_branch);
            let session = session.unwrap_or_else(|| task.clone());
            let command = match command {
                Some(json) => match serde_json::from_str::<Vec<String>>(&json) {
                    Ok(command) => Some(command),
                    Err(error) => {
                        eprintln!(
                            "radian arc work: --command is not a JSON array of strings: {error}"
                        );
                        return ExitCode::from(2);
                    }
                },
                None => None,
            };
            ExitCode::from(radian_arc::work::run(radian_arc::work::Options {
                task,
                attempt,
                token,
                session,
                resume,
                allow_noop,
                remote,
                main_branch,
                extra_args: extra,
                command,
                flow: flow.unwrap_or_else(|| radian_arc::store::config::DEFAULT_FLOW.to_owned()),
                worker_command,
                stall_timeout_secs: stall_timeout_secs.unwrap_or(0),
                max_wall_time_secs: max_wall_time_secs.unwrap_or(0),
                cancel_grace_secs: cancel_grace_secs.unwrap_or(0),
            }) as u8)
        }
        Some(ArcCommand::Gate { task, lock_fd }) => {
            ExitCode::from(radian_arc::gate::background::run(
                radian_arc::gate::background::Options { task, lock_fd },
            ) as u8)
        }
        // `run --watch` is `radian arc serve` (ADR 0048 §13), with or without `--interval`.
        // The old form is refused with one exact line and the usage exit code; the hidden
        // flags above exist only so that line is this command's own, not clap's.
        Some(ArcCommand::Run { watch: true, .. }) => {
            eprintln!("radian arc: `run --watch` is now `radian arc serve`");
            ExitCode::from(2)
        }
        Some(command) => {
            // `work` returned `None` above: every remaining subcommand has a store operation.
            let command = command
                .into_store_command()
                .expect("`work` and `gate` are dispatched above");
            ExitCode::from(radian_arc::store::cli::run(&command))
        }
        // A bare `radian arc` opens the TUI (ADR 0048 §1). Without setup the one-line refusal
        // names `radian arc init` before the terminal is entered.
        None => run_arc_tui(config),
    }
}

/// The wrapper's internal remote and main branch: an explicit flag wins, then the
/// repository's `config.toml`, then the git defaults.
fn work_remote_and_main(remote: Option<String>, main_branch: Option<String>) -> (String, String) {
    let names = std::env::current_dir()
        .ok()
        .and_then(|dir| radian_arc::store::Store::open(&dir).ok())
        .and_then(|store| store.repository_names().ok());
    let (config_remote, config_main) =
        names.unwrap_or_else(|| ("origin".to_owned(), "main".to_owned()));
    (
        remote.unwrap_or(config_remote),
        main_branch.unwrap_or(config_main),
    )
}

/// The `radian theme` subcommands.
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
/// it is the same dispatch). `config` is the top-level `--config`, for the one subcommand that
/// reads it (`radian arc`'s theme).
fn run_subcommand(command: RadianCommand, config: Option<&Path>) -> ExitCode {
    match command {
        RadianCommand::Exec { args } => {
            // The agent parser owns `--help`/`--version`/usage errors: `parse_from` exits
            // inside clap for those, exactly as the `agent` binary does.
            // The program name clap shows in usage and errors: `radian exec`, the command
            // the user ran.
            ExitCode::from(radian_exec::parse_and_run(
                std::iter::once("radian exec".to_string()).chain(args),
            ) as u8)
        }
        RadianCommand::Acp { args } => {
            // The `acp` parser owns `--help`/`--version`/usage errors, like `exec`.
            ExitCode::from(radian_exec::parse_and_run_acp(
                std::iter::once("radian acp".to_string()).chain(args),
            ) as u8)
        }
        // The management commands build the same `Cli` the `agent` parser would and run the
        // same dispatch (ADR 0032): `radian login` and `radian exec login` are
        // one code path, not two.
        RadianCommand::Login { provider } => {
            run_radian_exec(radian_exec::Cli::for_command(radian_exec::Command::Login {
                provider,
            }))
        }
        RadianCommand::Logout { provider, name } => run_radian_exec(radian_exec::Cli::for_command(
            radian_exec::Command::Logout { provider, name },
        )),
        RadianCommand::Memory { workspace, command } => {
            run_radian_exec(radian_exec::Cli::for_memory(
                workspace.map(|path| path.to_string_lossy().into_owned()),
                command,
            ))
        }
        RadianCommand::Trust { workspace, command } => {
            run_radian_exec(radian_exec::Cli::for_trust(
                workspace.map(|path| path.to_string_lossy().into_owned()),
                command,
            ))
        }
        RadianCommand::Config { command } => run_radian_exec(radian_exec::Cli::for_command(
            radian_exec::Command::Config { command },
        )),
        RadianCommand::Theme { command } => run_theme(command),
        RadianCommand::Arc { command } => run_arc(command, config),
        RadianCommand::Footer { command } => run_footer(command),
        RadianCommand::Docs { topic } => run_docs(topic.as_deref()),
        RadianCommand::Attach {
            session_id,
            follow,
            session_dir,
        } => run_attach(session_dir.as_deref(), session_id.as_deref(), follow),
        // Reserved names (ADR 0032): the shape is fixed now so scripts and docs can use it.
        // ADR 0043 keeps the control port but has no `serve`/`mcp` server mode, so nothing
        // implements these names; a usage-class refusal (2), never a silent no-op.
        RadianCommand::Serve => reserved("serve"),
        RadianCommand::Mcp => reserved("mcp"),
    }
}

/// Runs `radian docs [topic]`: the bundled manual from the same embedded table `read`
/// sees, so the two never drift. No arguments prints one line per topic (`topic: description`);
/// a topic prints that page verbatim. Read-only, no network, no model, no credentials.
/// An unknown topic exits 2 and lists the pages on stderr.
fn run_docs(topic: Option<&str>) -> ExitCode {
    match topic {
        None => {
            for (name, text) in radian_coding::docs::DOCS {
                let topic = radian_coding::docs::topic(name);
                let description = radian_coding::docs::description(text);
                let line = if description.is_empty() {
                    topic.to_owned()
                } else {
                    format!("{topic}: {description}")
                };
                if let Err(error) = radian_host::write_line(&line) {
                    return stdout_error(error);
                }
            }
            ExitCode::SUCCESS
        }
        Some(topic) => match radian_coding::docs::lookup_topic(topic) {
            Some((_, text)) => {
                if let Err(error) = radian_host::write_stdout(text) {
                    return stdout_error(error);
                }
                ExitCode::SUCCESS
            }
            None => {
                eprintln!(
                    "radian: unknown docs topic `{topic}`; available topics: {}",
                    radian_coding::docs::DOCS
                        .iter()
                        .map(|(name, _)| radian_coding::docs::topic(name))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                ExitCode::from(2)
            }
        },
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
                    if let Err(error) =
                        radian_host::write_line(&format!("wrote {}", written.display()))
                    {
                        return stdout_error(error);
                    }
                    for line in &converted.report {
                        if let Err(error) = radian_host::write_line(line) {
                            return stdout_error(error);
                        }
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

/// Runs `radian footer export <preset>` (ADR 0027): prints the
/// preset's TOML exactly (exit 0). An unknown preset names the four on stderr (exit 2,
/// a usage error, ADR 0032 A3).
fn run_footer(command: FooterCommand) -> ExitCode {
    match command {
        FooterCommand::Export { preset } => match radian_tui::footer::export_preset(&preset) {
            Ok(toml) => {
                if let Err(error) = radian_host::write_stdout(toml) {
                    return stdout_error(error);
                }
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("radian: {error}");
                ExitCode::from(2)
            }
        },
    }
}

/// Maps a stdout write failure to its exit code: a closed pipe is a normal stop (like
/// `head`, `git` and `ripgrep`), so the command stops writing and exits 0 with nothing on
/// stderr; any other write error is reported once on stderr and exits 1. Output bytes are
/// unchanged when the pipe stays open.
fn stdout_error(error: radian_host::StdoutError) -> ExitCode {
    match error {
        radian_host::StdoutError::BrokenPipe => ExitCode::SUCCESS,
        radian_host::StdoutError::Failed(error) => {
            eprintln!("radian: cannot write to stdout: {error}");
            ExitCode::from(1)
        }
    }
}

/// Refuses a reserved subcommand: one line on stderr and the usage-class exit code.
fn reserved(name: &str) -> ExitCode {
    eprintln!("radian: `{name}` is reserved; not implemented yet");
    ExitCode::from(2)
}

/// Runs one management command through the `radian-exec` dispatch and returns its exit code.
fn run_radian_exec(cli: radian_exec::Cli) -> ExitCode {
    ExitCode::from(radian_exec::run_blocking(cli) as u8)
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
    // A subcommand is a mode of its own: it never enters the TUI and returns its own exit
    // code (ADR 0032). The top-level `--config` is passed along because `radian arc` reads the
    // same file the radian TUI does for its theme.
    if let Some(command) = cli.command.take() {
        let config = cli.config.clone();
        return run_subcommand(command, config.as_deref());
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
/// the text stays drawn where the cursor happened to be (the input box) and never clears. On that path the lines are collected here and appended to the transcript's
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
fn run(
    cli: Cli,
    runtime: &tokio::runtime::Runtime,
    rx: Receiver<UiEvent>,
    tx: Sender<UiEvent>,
) -> Result<Option<String>, RunError> {
    let Some(startup) = run_startup::startup_preamble(&cli, &tx)? else {
        return Ok(None);
    };
    let run_startup::Startup {
        dir,
        workspace_root,
        workspace_path,
        config,
        config_warnings,
        mut app,
        allow_unix_sockets,
        command_specs,
        mouse,
        mut startup_terminal,
        startup_log,
        mut resume,
        pick_notice,
        startup_picker,
    } = startup;
    match run_startup::run_picker_phase(
        &mut app,
        startup_picker,
        &mut startup_terminal,
        resume,
        &cli,
        &config,
        &rx,
        &tx,
    )? {
        run_startup::PickDone::Leave(reason) => return Ok(reason),
        run_startup::PickDone::Continue { resume: next } => {
            resume = next;
        }
    }

    // `/new` re-runs one session per iteration in the same terminal: the preamble and the
    // picker happen once; each iteration opens one session and either exits with its reason
    // (`Done`) or carries the values in force into a fresh session (`Again`). The terminal
    // pair lives across iterations; each iteration gets its own app and startup log.
    let mut term = startup_terminal;
    let mut app = app;
    let mut apply_warnings: Vec<String> = Vec::new();
    let mut carry: Option<run_resolve::SessionCarry> = None;
    // The preamble's log belongs to the first session (`--pick` collected diagnostics in
    // it); later sessions start a fresh collected log — the screen is up.
    let mut first_log = Some(startup_log);
    loop {
        let first = carry.is_none();
        let resume_for = if first { resume.clone() } else { None };
        let startup_log = first_log.take().unwrap_or(StartupLog {
            deferred: Some(Vec::new()),
        });
        match run_one_session(
            &cli,
            runtime,
            &rx,
            &tx,
            &dir,
            &workspace_root,
            &workspace_path,
            &config,
            &config_warnings,
            &command_specs,
            mouse,
            &allow_unix_sockets,
            resume_for,
            carry.take(),
            pick_notice && first,
            std::mem::take(&mut apply_warnings),
            app,
            startup_log,
            &mut term,
        ) {
            Ok(NextSession::Done(reason)) => return Ok(reason),
            Ok(NextSession::Again(next)) => {
                carry = Some(next.carry);
                apply_warnings = next.apply_warnings;
                app = next.app;
            }
            Err(error) => {
                // The terminal is entered from the second session on; an error after that
                // must leave it restored (idempotent, and `Drop` restores anyway).
                if let Some((guard, _)) = term.as_mut() {
                    guard.restore();
                }
                return Err(error);
            }
        }
    }
}

/// What `run()` does after one session: exit with the reason, or open the next session in
/// the same terminal with a fresh view.
enum NextSession {
    Done(Option<String>),
    Again(Box<NextCarry>),
}

/// A `/new` handoff: the carried values, the next session's fresh app, and the warnings
/// its re-applied config produced (for the next session's startup block).
struct NextCarry {
    carry: run_resolve::SessionCarry,
    app: App,
    apply_warnings: Vec<String>,
}

/// One iteration of `run()`: resolve, open, wire, screen and loop exactly one session.
/// A carried `/new` resolves with no resume (a fresh id) and the carried values; the
/// terminal pair is reused when present, entered when absent. Returns either the exit or
/// the next iteration's app (a fresh view: transcript, footer counts and process-local
/// history reset; the chrome is re-applied from the same config).
#[allow(clippy::too_many_arguments)]
fn run_one_session(
    cli: &Cli,
    runtime: &tokio::runtime::Runtime,
    rx: &Receiver<UiEvent>,
    tx: &Sender<UiEvent>,
    dir: &Path,
    workspace_root: &radian_tui::workspace::CodingRoot,
    workspace_path: &Path,
    config: &radian_host::Config,
    config_warnings: &[String],
    command_specs: &[radian_host::CommandSpec],
    mouse: bool,
    allow_unix_sockets: &[PathBuf],
    resume: Option<String>,
    carry: Option<run_resolve::SessionCarry>,
    pick_notice: bool,
    apply_warnings: Vec<String>,
    mut app: App,
    mut startup_log: StartupLog,
    term: &mut Option<(
        radian_tui::terminal::TtyGuard,
        Terminal<CrosstermBackend<std::io::Stdout>>,
    )>,
) -> Result<NextSession, RunError> {
    // The carried values ride above the command line; a fresh session collects its
    // warnings into the startup block (the normal screen is gone once `/new` has run).
    let carry_auto = carry.as_ref().is_some_and(|carry| carry.auto);
    for warning in apply_warnings {
        startup_log.line(format!("[config] {warning}"));
    }
    let run_resolve::ResolvedModel {
        model_name,
        has_model,
        tier,
        access_source,
        provider_name,
        provider_ref,
        key_id,
        protocol,
        provider,
        chatgpt_oauth,
        thinking,
        thinking_budget,
        thinking_for_footer,
        credential,
        base_url_override,
        header_args,
        catalog,
        resumed_tier,
        cli_tier,
    } = run_resolve::resolve_model(
        cli,
        config,
        dir,
        workspace_path,
        resume.as_deref(),
        carry.as_ref(),
        &mut startup_log,
    )?;
    // The window/route phase borrows the model/provider answer and returns the session
    // inputs; the fields later phases still need come back out here, so the rest of
    // `run()` keeps its names.
    let resolved_model = run_resolve::ResolvedModel {
        model_name,
        has_model,
        tier,
        access_source,
        provider_name,
        provider_ref,
        key_id,
        protocol,
        provider,
        chatgpt_oauth,
        thinking,
        thinking_budget,
        thinking_for_footer,
        credential,
        base_url_override,
        header_args,
        catalog,
        resumed_tier,
        cli_tier,
    };
    let run_resolve::ResolvedRoute {
        window,
        session_id,
        secrets,
        route,
        replays_reasoning,
        settings,
        config_slot,
        model,
        budget_for,
        route_price,
    } = run_resolve::resolve_window_and_route(
        cli,
        config,
        runtime,
        &mut app,
        &resume,
        carry.as_ref(),
        &resolved_model,
        &mut startup_log,
    )?;
    let run_resolve::ResolvedModel {
        model_name,
        has_model,
        tier,
        access_source,
        provider_name,
        provider_ref,
        thinking_for_footer,
        resumed_tier,
        cli_tier,
        ..
    } = resolved_model;
    let (
        run_open::SessionInputs {
            sandbox,
            inherited_sandbox,
            access_builder,
            bash_sandbox,
            servers,
            startup_approval,
            profile,
            projection,
            control,
            live,
            approvals,
            audit,
            runtime_events,
            driver_stopped,
            before_driver,
            memory_warning,
            hooks_warning,
            wake_warning,
            intent_stop_warning,
            notice_lost_warning,
            trust_setup,
            project_hooks,
            memory_root,
        },
        _control_guard,
    ) = run_open::build_session_inputs(
        cli,
        config,
        runtime,
        tx,
        dir,
        workspace_path,
        &mut app,
        &resume,
        tier,
        carry_auto,
        allow_unix_sockets,
        &session_id,
        &secrets,
        &mut startup_log,
    )?;

    // The session-opening phase takes the input and route structs and returns the open
    // session. Later phases keep their own names: the values this consumes but later
    // phases still borrow (`window`, `route_price`) are cloned into the call, and the
    // inputs this never touches (`startup_approval`, `inherited_sandbox`, `bash_sandbox`)
    // stay out of it.
    let mut session = run_open::open_session_and_refresh(
        runtime,
        cli,
        run_open::SessionInputs {
            sandbox,
            servers,
            profile,
            projection,
            control,
            live,
            approvals,
            audit,
            runtime_events,
            driver_stopped,
            before_driver,
            memory_warning,
            hooks_warning,
            wake_warning,
            intent_stop_warning,
            notice_lost_warning,
            trust_setup,
            project_hooks,
            memory_root,
            access_builder,
            startup_approval: startup_approval.clone(),
            inherited_sandbox,
            bash_sandbox: bash_sandbox.clone(),
        },
        run_resolve::ResolvedRoute {
            window: window.clone(),
            session_id: session_id.clone(),
            secrets: secrets.clone(),
            route,
            replays_reasoning,
            settings,
            config_slot,
            model,
            budget_for,
            route_price: route_price.clone(),
        },
        dir,
        workspace_path,
        &session_id,
        &resume,
        tier,
        &provider_name,
        &model_name,
        &provider_ref,
        &config.providers,
    )?;
    let run_ready::PostOpen {
        usage_gate,
        chatgpt_usage_gate,
        background,
    } = run_ready::wire_post_open(
        cli,
        config,
        runtime,
        tx,
        &session,
        &mut app,
        carry.as_ref(),
        workspace_path,
        &provider_name,
        &model_name,
        thinking_for_footer,
        tier,
        access_source,
        resumed_tier,
        cli_tier,
        inherited_sandbox,
        &startup_approval,
        &window,
        &route_price,
        &bash_sandbox,
        command_specs.to_vec(),
        &session_id,
        &mut startup_log,
    );
    let run_ready::TerminalReady {
        guard,
        mut terminal,
        memory_service,
        history,
        queue_wake,
    } = run_ready::enter_terminal_and_history(
        cli,
        runtime,
        tx,
        dir,
        mouse,
        term.take(),
        &mut session,
    )?;
    let herdr = run_finish::build_startup_screen(
        cli,
        &mut app,
        &session,
        &history,
        dir,
        &session_id,
        &resume,
        workspace_root,
        workspace_path,
        &window,
        &model_name,
        has_model,
        tier,
        access_source,
        config,
        &startup_approval,
        pick_notice,
        config_warnings,
        tx,
        &mut startup_log,
    );
    // A carried `/new` lands here: name the fresh session and how to resume the old one.
    // The old session is still open at this point, so its id (but not its journal) is all
    // the notice needs.
    if let Some(previous) = carry.as_ref().map(|carry| carry.old_session_id.clone()) {
        app.push_notice(format!(
            "new session {session_id}; {previous} is saved — {}",
            resume_command(&program_name(), &previous, cli, workspace_path)
        ));
    }

    match run_finish::run_loop_and_teardown(
        cli,
        config,
        runtime,
        rx,
        tx,
        &mut app,
        session,
        &mut terminal,
        guard,
        &memory_service,
        workspace_path,
        &provider_name,
        &model_name,
        &route_price,
        bash_sandbox,
        usage_gate,
        chatgpt_usage_gate,
        background,
        queue_wake,
        herdr,
        dir,
        &session_id,
        &secrets,
        has_model,
    )? {
        run_finish::SessionEnd::Done(reason) => Ok(NextSession::Done(reason)),
        run_finish::SessionEnd::New(new) => {
            // The guard was never restored: reassemble the terminal pair for the next
            // session. A fresh view resets the transcript, the footer counts and the
            // process-local history; the chrome is re-applied from the same config.
            *term = Some((new.guard, terminal));
            let mut app = App::new(String::new(), String::new());
            app.set_workspace(workspace_path.to_path_buf());
            let apply_warnings = app
                .apply_config(config, radian_tui::theme::Theme::no_color_env())
                .map_err(|error| format!("config: {error}"))?;
            Ok(NextSession::Again(Box::new(NextCarry {
                carry: new.carry,
                app,
                apply_warnings,
            })))
        }
    }
}

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
/// it — the key comes from the credential store and the headers from the config file.
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

/// Looks up a route's catalog prices from the warm local cache (ADR 0026 D7, ADR 0027
/// ). Offline by construction: a switch never fetches, and a miss means "no price".
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

#[cfg(test)]
mod tests {
    use super::*;
    use radian_coding::drift::Table as DocTable;

    #[test]
    fn the_default_model_call_limit_is_the_tui_one_not_the_cores() {
        let cli = Cli::try_parse_from(["radian", "--model", "m"]).unwrap();
        assert_eq!(cli.max_model_calls, TUI_DEFAULT_MAX_MODEL_CALLS);
        assert_eq!(TUI_DEFAULT_MAX_MODEL_CALLS, 1000);
        let cli =
            Cli::try_parse_from(["radian", "--model", "m", "--max-model-calls", "5"]).unwrap();
        assert_eq!(cli.max_model_calls, 5);
    }

    /// User-facing `--help` text never names an ADR: render `radian --help` and every
    /// subcommand's help and assert none contains `ADR`.
    /// Mutation: put `(ADR 0008)` back into one help string → this fails.
    #[test]
    fn help_text_names_no_adr() {
        use clap::CommandFactory;
        fn collect(command: &clap::Command, out: &mut Vec<String>) {
            let mut command = command.clone();
            out.push(command.render_long_help().to_string());
            for sub in command.get_subcommands() {
                collect(sub, out);
            }
        }
        let mut pages = Vec::new();
        collect(&Cli::command(), &mut pages);
        assert!(pages.len() > 4, "the tree has a root and subcommands");
        for page in &pages {
            assert!(!page.contains("ADR"), "help names an ADR:\n{page}");
        }
    }

    /// The short aliases ("常用命令都有 shorthand") are the long flags: each
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

    /// Every long option of every `radian` subcommand appears in `docs/user/cli.md`.
    /// Mutation: delete one `--flag` row from cli.md → this fails naming it.
    /// Why the clap tree is the source that cannot drift: it is the parser itself, so a
    /// renamed or added flag fails here without any list to update. Hidden flags are test
    /// seams, not user surface, so they are not required.
    #[test]
    fn every_long_option_appears_in_cli_md() {
        use clap::CommandFactory;
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let cli_md = std::fs::read_to_string(manifest.join("../../docs/user/cli.md"))
            .expect("cli.md exists");
        fn collect(command: &clap::Command, out: &mut Vec<String>) {
            for arg in command.get_arguments() {
                if arg.is_hide_set() {
                    continue;
                }
                if let Some(long) = arg.get_long() {
                    out.push(format!("--{long}"));
                }
            }
            for sub in command.get_subcommands() {
                // `radian arc` has its own page: its options are checked against `arc/index.md`.
                if sub.get_name() == "arc" {
                    continue;
                }
                collect(sub, out);
            }
        }
        let mut options = Vec::new();
        collect(&Cli::command(), &mut options);
        assert!(!options.is_empty(), "the tree has options");
        let missing: Vec<_> = options
            .iter()
            .filter(|option| !cli_md.contains(option.as_str()))
            .collect();
        assert!(
            missing.is_empty(),
            "these long options are missing from docs/user/cli.md: {missing:?}"
        );
    }

    /// Every closed CLI value set appears in its option's rows of `docs/user/cli.md`, and
    /// every defaulted option's default appears in its row.
    /// Mutation: drop one value from a `--thinking` row → this fails naming the flag.
    /// Why the code is the source that cannot drift: the possible values and defaults come
    /// from the clap tree itself (the parser), and the stringly closed sets are the `pub
    /// const`s the parsers read. Hidden flags are test seams, not user surface, so they are
    /// not required; clap's own `--help`/`--version` are documented once in the TUI table.
    #[test]
    fn cli_option_values_and_defaults_appear_in_cli_md() {
        use clap::CommandFactory;
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let cli_md = std::fs::read_to_string(manifest.join("../../docs/user/cli.md"))
            .expect("cli.md exists");
        fn collect(command: &clap::Command, out: &mut Vec<(String, Vec<String>, Vec<String>)>) {
            for arg in command.get_arguments() {
                if arg.is_hide_set() {
                    continue;
                }
                let id = arg.get_id();
                if id == "help" || id == "version" {
                    continue;
                }
                if let Some(long) = arg.get_long() {
                    // Flags take no value: presence is the value, so there is no closed
                    // set or default to check. Options that take a value report both.
                    let takes_value = arg.get_action().takes_values();
                    let values: Vec<String> = if takes_value {
                        arg.get_possible_values()
                            .iter()
                            .map(|value| value.get_name().to_owned())
                            .collect()
                    } else {
                        Vec::new()
                    };
                    let defaults: Vec<String> = if takes_value {
                        arg.get_default_values()
                            .iter()
                            .map(|value| value.to_string_lossy().into_owned())
                            .collect()
                    } else {
                        Vec::new()
                    };
                    out.push((format!("--{long}"), values, defaults));
                }
            }
            for sub in command.get_subcommands() {
                // `radian arc` has its own page: its values and defaults are checked
                // against the arc pages (see `every_arc_option_and_value_appears_in_the_arc_manual`).
                if sub.get_name() == "arc" {
                    continue;
                }
                collect(sub, out);
            }
        }
        let mut options = Vec::new();
        collect(&Cli::command(), &mut options);
        assert!(!options.is_empty(), "the tree has options");
        let tables = cli_option_tables(&cli_md);
        for (flag, values, defaults) in &options {
            let rows = rows_with_flag(&tables, flag);
            if rows.is_empty() {
                // Prose-documented flags (attach, footer export, theme import, config
                // import, memory `--workspace`): presence only, no value set to check.
                assert!(
                    cli_md.contains(flag.as_str()),
                    "{flag} is missing from docs/user/cli.md"
                );
                continue;
            }
            for row in &rows {
                for value in values {
                    assert!(
                        row.iter().any(|cell| cell.contains(value.as_str())),
                        "{flag} row is missing value {value}: {row:?}"
                    );
                }
                for default in defaults {
                    assert!(
                        default_in_row(row, default),
                        "{flag} row is missing default {default}: {row:?}"
                    );
                }
            }
        }
        // Stringly closed sets: the parser's own consts, checked against every row.
        let stringly: &[(&str, Vec<String>)] = &[
            (
                "--thinking",
                radian_host::THINKING_LEVELS
                    .iter()
                    .map(|level| level.to_string())
                    .collect(),
            ),
            (
                "--profile",
                radian_permissions::TIER_NAMES
                    .iter()
                    .chain(radian_permissions::TIER_ALIASES.iter())
                    .map(|name| name.to_string())
                    .collect(),
            ),
            (
                "--approval",
                radian_permissions::APPROVAL_POLICIES
                    .iter()
                    .map(|policy| policy.to_string())
                    .collect(),
            ),
        ];
        for (flag, values) in stringly {
            let rows = rows_with_flag(&tables, flag);
            assert!(!rows.is_empty(), "{flag} has no row in cli.md");
            for row in &rows {
                for value in values {
                    assert!(
                        row.iter().any(|cell| cell.contains(value.as_str())),
                        "{flag} row is missing value {value}: {row:?}"
                    );
                }
            }
        }
        // The `--thinking` default is the code's.
        for row in rows_with_flag(&tables, "--thinking") {
            assert!(
                default_in_row(row, radian_host::DEFAULT_THINKING_LEVEL),
                "--thinking row misses its default: {row:?}"
            );
        }
    }

    // ---- the arc manual --------------------------------------------------------------
    //
    // The arc manual is five pages: `arc/index.md` (the commands), `arc/loop.md` (the scheduling
    // loop, its pass and `loop.log`), `arc/gate.md` (the gate and failures), `arc/mcp.md`
    // (the lead's MCP server) and `arc/files.md` (the config, the plan and task files, the
    // layout).
    // Every check locates its table by the heading it sits under and the page that holds
    // it. The heading is the anchor: `arc_tables(doc, "arc/index.md", "## `radian arc show")` is
    // the only way a check reads rows, so an unrelated table elsewhere on any page can
    // neither satisfy nor break it. A missing heading (or a heading with no table) fails
    // with the page and the heading.

    /// Every data table under the heading that starts with `prefix`, from that heading to
    /// the next heading at the same or a higher level. The heading is the table's anchor:
    /// a table outside it is never read by a check.
    ///
    /// The reader is `radian_coding::drift`, shared with the `radian-host`, `radian-exec`
    /// and `radian-tui` footer checks: one heading-anchor convention serves every manual
    /// check.
    fn doc_tables(doc: &str, page: &str, prefix: &str) -> Vec<DocTable> {
        radian_coding::drift::tables_under(doc, page, prefix)
    }

    /// Every data table under a heading of one arc page.
    fn arc_tables(doc: &str, page: &str, prefix: &str) -> Vec<DocTable> {
        doc_tables(doc, page, prefix)
    }

    /// The first table under `prefix` of one arc page.
    fn arc_table(doc: &str, page: &str, prefix: &str) -> DocTable {
        arc_tables(doc, page, prefix).swap_remove(0)
    }

    /// The `cli.md` tables the option checks read. Each heading is an anchor: a table
    /// elsewhere on the page is never read.
    const CLI_OPTION_HEADINGS: &[&str] = &[
        "### TUI options",
        "### Model options",
        "### Session options",
        "### MCP options",
        "### Permission options",
        "## `radian exec`",
        "## `radian memory`",
        "## `radian trust`",
    ];

    fn cli_option_tables(doc: &str) -> Vec<DocTable> {
        CLI_OPTION_HEADINGS
            .iter()
            .flat_map(|heading| doc_tables(doc, "cli.md", heading))
            .collect()
    }

    /// A row's first cell.
    fn first_cell(row: &[String]) -> &str {
        row.first().map(String::as_str).unwrap_or("")
    }

    /// The row whose first cell is exactly `cell`.
    fn row_with_first_cell<'a>(table: &'a DocTable, cell: &str) -> Option<&'a Vec<String>> {
        table.iter().find(|row| first_cell(row) == cell)
    }

    /// The rows whose first cell names `needle`.
    fn rows_with_flag<'a>(tables: &'a [DocTable], needle: &str) -> Vec<&'a Vec<String>> {
        tables
            .iter()
            .flatten()
            .filter(|row| row.first().is_some_and(|cell| cell.contains(needle)))
            .collect()
    }

    /// The argument name of a table's first cell, when it is a positional (`<ID>`).
    fn positional_name(cell: &str) -> Option<String> {
        let cell = cell.trim().trim_matches('`');
        let inner = cell.strip_prefix('<')?.strip_suffix('>')?;
        Some(inner.strip_suffix("...").unwrap_or(inner).to_owned())
    }

    /// Whether `default` sits in the row's Default cell (the Allowed/Value cells would mask
    /// a changed default). Tables without a Default column fall back to the whole row.
    fn default_in_row(row: &[String], default: &str) -> bool {
        if row.len() >= 4 {
            row[2].contains(default)
        } else {
            row.iter().any(|cell| cell.contains(default))
        }
    }

    /// One arc manual page: its file name and text.
    fn arc_page(name: &str) -> String {
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let path = format!("../../docs/user/{name}");
        std::fs::read_to_string(manifest.join(path)).unwrap_or_else(|error| {
            panic!("docs/user/{name} exists: {error}");
        })
    }

    /// `docs/user/arc/index.md`, the page the `radian arc` commands are checked against.
    fn arc_md() -> String {
        arc_page("arc/index.md")
    }

    /// `docs/user/arc/loop.md`, the page the scheduling loop and its log are checked against.
    fn arc_loop_md() -> String {
        arc_page("arc/loop.md")
    }

    /// `docs/user/arc/gate.md`, the page the gate and failure sections are checked against.
    fn arc_gate_md() -> String {
        arc_page("arc/gate.md")
    }

    /// `docs/user/arc/mcp.md`, the page the lead's MCP server is checked against.
    fn arc_mcp_md() -> String {
        arc_page("arc/mcp.md")
    }

    /// `docs/user/arc/files.md`, the page the config and file sections are checked against.
    fn arc_files_md() -> String {
        arc_page("arc/files.md")
    }

    /// The `arc` subcommand of the parser, as a tree of its own.
    fn arc_subcommand() -> clap::Command {
        use clap::CommandFactory;
        Cli::command()
            .find_subcommand("arc")
            .expect("`radian arc` exists")
            .clone()
    }

    /// Every subcommand the parser has is in `SUBCOMMANDS` and on the page, and every name
    /// the page calls not built is in `NOT_BUILT` too. The parser is the source: a renamed
    /// or added subcommand fails here.
    #[test]
    fn every_arc_subcommand_appears_in_arc_md() {
        let doc = arc_md();
        let arc = arc_subcommand();
        // `work` is the internal wrapper: hidden from `--help` and not documented (AGENTS.md:
        // an input that exists only for an internal child process is not in the manual).
        let visible = |command: &&clap::Command| !command.is_hide_set();
        let mut parsed: Vec<&str> = arc
            .get_subcommands()
            .filter(visible)
            .map(|sub| sub.get_name())
            .collect();
        parsed.sort();
        let mut listed: Vec<&str> = radian_arc::store::cli::SUBCOMMANDS.to_vec();
        listed.sort();
        assert_eq!(parsed, listed, "the parser and `SUBCOMMANDS` agree");
        for sub in arc.get_subcommands().filter(visible) {
            let name = sub.get_name();
            assert!(
                doc.contains(&format!("radian arc {name}")),
                "`radian arc {name}` is missing from arc/index.md"
            );
            for nested in sub.get_subcommands() {
                assert!(
                    doc.contains(&format!("radian arc {name} {}", nested.get_name())),
                    "`radian arc {name} {}` is missing from arc/index.md",
                    nested.get_name()
                );
            }
        }
        for name in radian_arc::store::cli::NOT_BUILT {
            assert!(
                doc.contains(&format!("`{name}`")),
                "arc/index.md does not name the unbuilt subcommand `{name}`"
            );
        }
    }

    /// The three path prefixes of the "changed arc's code" line are one list in the code, and
    /// the loop page names every one of them. A prefix added to the list fails here until the
    /// page lists it.
    #[test]
    fn every_arc_code_prefix_appears_in_arc_loop_md() {
        let doc = arc_loop_md();
        for prefix in radian_arc::run::ARC_CODE_PREFIXES {
            assert!(
                doc.contains(&format!("`{prefix}`")),
                "arc/loop.md does not name the arc-code prefix `{prefix}`"
            );
        }
    }

    /// The review item's levels are one closed list in the code, and the gate page names every
    /// one of them and nothing else. A level added to the list fails here until the page lists
    /// it.
    #[test]
    fn every_review_level_appears_in_arc_gate_md() {
        let doc = arc_gate_md();
        let listed = radian_arc::gate::REVIEW_LEVELS
            .iter()
            .map(|level| format!("`{level}`"))
            .collect::<Vec<_>>()
            .join(", ");
        assert!(
            doc.contains(&format!("`level` is one of {listed}")),
            "arc/gate.md does not say `level` is one of {listed}"
        );
        // The report's example is the object form the parser reads.
        assert!(
            doc.contains("\"blocking\": [{\"level\""),
            "arc/gate.md's report example is not the object form"
        );
    }

    /// The review stage's failure reasons are one closed list in the code, and the gate page
    /// names every one of them: a reason added to the list fails here until the page lists it.
    #[test]
    fn every_review_failure_reason_appears_in_arc_gate_md() {
        let doc = arc_gate_md();
        for reason in radian_arc::gate::REVIEW_FAILURE_REASONS {
            assert!(
                doc.contains(&format!("`{reason}`")),
                "arc/gate.md does not name the review failure reason `{reason}`"
            );
        }
    }

    /// One option of one arc command.
    struct ArcOptionRow {
        flag: String,
        values: Vec<String>,
        defaults: Vec<String>,
    }

    /// One arc command with the heading that anchors its manual tables.
    struct ArcCommandRow {
        heading: String,
        options: Vec<ArcOptionRow>,
        positionals: Vec<String>,
    }

    /// Every visible arc command that takes an option or a positional, with the heading its
    /// manual tables sit under. The parser is the source: a renamed or added argument fails
    /// the check without a list to update.
    fn collect_arc_commands() -> Vec<ArcCommandRow> {
        fn walk(command: &clap::Command, path: &mut Vec<String>, out: &mut Vec<ArcCommandRow>) {
            if command.is_hide_set() {
                return;
            }
            if !path.is_empty() {
                let mut options = Vec::new();
                let mut positionals = Vec::new();
                for arg in command.get_arguments() {
                    if arg.is_hide_set() {
                        continue;
                    }
                    let id = arg.get_id().to_string();
                    if id == "help" || id == "version" {
                        continue;
                    }
                    if let Some(long) = arg.get_long() {
                        let takes_value = arg.get_action().takes_values();
                        let values = if takes_value {
                            arg.get_possible_values()
                                .iter()
                                .map(|value| value.get_name().to_owned())
                                .collect()
                        } else {
                            Vec::new()
                        };
                        let defaults = if takes_value {
                            arg.get_default_values()
                                .iter()
                                .map(|value| value.to_string_lossy().into_owned())
                                .collect()
                        } else {
                            Vec::new()
                        };
                        options.push(ArcOptionRow {
                            flag: format!("--{long}"),
                            values,
                            defaults,
                        });
                    } else if arg.is_positional() {
                        match arg.get_value_names() {
                            Some(names) => positionals
                                .extend(names.iter().map(|name| name.as_str().to_owned())),
                            None => positionals.push(id),
                        }
                    }
                }
                if !options.is_empty() || !positionals.is_empty() {
                    let level = "#".repeat(path.len() + 1);
                    out.push(ArcCommandRow {
                        heading: format!("{level} `radian arc {}", path.join(" ")),
                        options,
                        positionals,
                    });
                }
            }
            for sub in command.get_subcommands() {
                path.push(sub.get_name().to_owned());
                walk(sub, path, out);
                path.pop();
            }
        }
        let mut out = Vec::new();
        walk(&arc_subcommand(), &mut Vec::new(), &mut out);
        out
    }

    /// Every option of `radian arc`, with every allowed value and default the parser
    /// declares, appears in the tables under that command's own heading, on the arc page
    /// that holds it (`arc/index.md`, or `arc/loop.md` for `run` and `serve`). Every positional
    /// the parser takes is a `<NAME>` row there, and every `<NAME>` row names a positional
    /// the parser takes (the reverse). A table elsewhere on the page is never read.
    #[test]
    fn every_arc_option_and_value_appears_in_the_arc_manual() {
        // `arc/loop.md` first: the overview page may name a command and link to its full
        // section, and the table lives with the section.
        let pages = [("arc/loop.md", arc_loop_md()), ("arc/index.md", arc_md())];
        let commands = collect_arc_commands();
        assert!(!commands.is_empty(), "the arc subcommand has commands");
        for command in &commands {
            let (page, doc) = pages
                .iter()
                .find(|(_, doc)| doc.contains(&command.heading))
                .unwrap_or_else(|| {
                    panic!(
                        "{} is missing from arc/index.md and arc/loop.md",
                        command.heading
                    )
                });
            let tables = arc_tables(doc, page, &command.heading);
            for option in &command.options {
                let rows = rows_with_flag(&tables, &option.flag);
                if rows.is_empty() {
                    // A flag documented in prose (init's `--track`) still has to be named
                    // on the page; the heading and the prose around the table are its
                    // documentation.
                    assert!(
                        doc.contains(&option.flag),
                        "{} is missing from {page}",
                        option.flag
                    );
                    continue;
                }
                for row in rows {
                    for value in &option.values {
                        assert!(
                            row.iter().any(|cell| cell.contains(value.as_str())),
                            "{} row is missing value {value}: {row:?}",
                            option.flag
                        );
                    }
                    for default in &option.defaults {
                        assert!(
                            default_in_row(row, default),
                            "{} row is missing default {default}: {row:?}",
                            option.flag
                        );
                    }
                }
            }
            let documented: Vec<String> = tables
                .iter()
                .flatten()
                .filter_map(|row| positional_name(first_cell(row)))
                .collect();
            for name in &command.positionals {
                assert!(
                    documented.contains(name),
                    "{} is missing the positional `<{name}>`",
                    command.heading
                );
            }
            for name in &documented {
                assert!(
                    command.positionals.contains(name),
                    "{} documents `<{name}>`, which the parser does not take",
                    command.heading
                );
            }
        }
    }

    /// Every arc exit code and its meaning appears in the table under `## Exit codes`, and
    /// that table holds exactly the code's closed list. A decoy table elsewhere on the page
    /// is never read.
    #[test]
    fn every_arc_exit_code_appears_in_arc_md() {
        let doc = arc_md();
        let table = arc_table(&doc, "arc/index.md", "## Exit codes");
        for (code, meaning) in radian_arc::store::EXIT_CODES {
            let cell = format!("`{code}`");
            let row = row_with_first_cell(&table, &cell).unwrap_or_else(|| {
                panic!("arc/index.md's `## Exit codes` table has no row for exit code {code}")
            });
            assert!(
                row.iter().any(|cell| cell.contains(meaning)),
                "the `{code}` row misses `{meaning}`: {row:?}"
            );
        }
        let mut documented = Vec::new();
        for row in &table {
            let code = first_cell(row)
                .trim_matches('`')
                .parse::<u8>()
                .unwrap_or_else(|_| {
                    panic!(
                        "arc/index.md's `## Exit codes` table has a row that is not a code: {row:?}"
                    )
                });
            documented.push(code);
        }
        let expected: Vec<u8> = radian_arc::store::EXIT_CODES
            .iter()
            .map(|(code, _)| *code)
            .collect();
        assert_eq!(
            documented, expected,
            "arc/index.md's exit-code table is the code's closed list"
        );
    }

    /// Every code `radian arc merge` itself returns appears in the table under
    /// `### Merge exit codes`, with the meaning `EXIT_CODES` gives it, and no other code
    /// does. The source is `MERGE_EXIT_CODES`, so the table cannot drift from the command.
    #[test]
    fn every_merge_exit_code_appears_in_arc_md() {
        let doc = arc_md();
        let table = arc_table(&doc, "arc/index.md", "### Merge exit codes");
        let mut documented = Vec::new();
        for row in &table {
            let code = first_cell(row)
                .trim_matches('`')
                .parse::<u8>()
                .unwrap_or_else(|_| {
                    panic!("arc/index.md's merge exit table has a row that is not a code: {row:?}")
                });
            documented.push(code);
            let meaning = radian_arc::store::EXIT_CODES
                .iter()
                .find(|(known, _)| *known == code)
                .unwrap_or_else(|| panic!("merge exit code {code} is not one of arc's exit codes"))
                .1;
            assert!(
                row.iter().any(|cell| cell.contains(meaning)),
                "the `{code}` merge row misses `{meaning}`: {row:?}"
            );
        }
        let mut expected: Vec<u8> = radian_arc::store::MERGE_EXIT_CODES
            .iter()
            .map(|exit| exit.code())
            .collect();
        documented.sort_unstable();
        expected.sort_unstable();
        assert_eq!(
            documented, expected,
            "arc/index.md's merge exit table is the command's own code list"
        );
    }

    /// Every `config.toml` key appears in the table under `## config.toml` on
    /// `arc/files.md` with the default the code has, and every key whose default is "not
    /// set" says so. The default is
    /// compared as a parsed TOML value, so the table may order an inline table's keys
    /// readably (`stage` before `required`) instead of following the serializer's order.
    /// The source is the serialized `Config::default()`, so a changed default fails here.
    #[test]
    fn every_config_key_and_default_appears_in_arc_files_md() {
        let doc = arc_files_md();
        let table = arc_table(&doc, "arc/files.md", "## `config.toml`");
        let config = radian_arc::store::config::Config::default();
        let value = toml::Value::try_from(config).expect("the config serializes to TOML");
        let mut leaves = Vec::new();
        flatten_toml("", &value, &mut leaves);
        assert!(leaves.iter().any(|(key, _)| key == "schema_version"));
        assert!(
            leaves
                .iter()
                .any(|(key, _)| key == "gate.profiles.default.stages")
        );
        for (key, rendered) in &leaves {
            let row = config_key_row(&table, key).unwrap_or_else(|| {
                panic!("arc/files.md's `## config.toml` table has no row for `{key}`")
            });
            let cell = config_default_cell(row);
            let expected = parse_toml_value(rendered).unwrap_or_else(|| {
                panic!("the code's `{key}` default `{rendered}` is not a TOML value")
            });
            let actual = parse_default_cell(cell)
                .unwrap_or_else(|| panic!("`{key}`'s Default cell `{cell}` is not a TOML value"));
            assert_eq!(
                actual, expected,
                "`{key}`'s Default cell is `{cell}`, but the code's default is `{rendered}`"
            );
        }
        for key in radian_arc::store::config::OPTIONAL_KEYS {
            let row = config_key_row(&table, key).unwrap_or_else(|| {
                panic!("arc/files.md's `## config.toml` table has no row for `{key}`")
            });
            assert!(
                config_default_cell(row).contains("not set"),
                "`{key}`'s Default cell must say `not set`: {row:?}"
            );
        }
    }

    /// The arguments the wrapper sets are the ones the `worker.exec_args` row names: the row's
    /// Allowed values cell lists exactly `radian_arc::worker_args::OWNED_EXEC_ARGS`, in order.
    /// The source is the code's one list, so a new or removed owned argument fails here, and
    /// so does a row that names an argument the wrapper does not own.
    #[test]
    fn the_owned_worker_arguments_are_the_config_rows_list() {
        let doc = arc_files_md();
        let table = arc_table(&doc, "arc/files.md", "## `config.toml`");
        let row = config_key_row(&table, "worker.exec_args")
            .expect("arc/files.md's config table has a `worker.exec_args` row");
        let allowed = row.get(3).expect("the row has an Allowed values cell");
        let documented: Vec<&str> = allowed
            .split('`')
            .skip(1)
            .step_by(2)
            .filter(|token| token.starts_with('-'))
            .collect();
        assert_eq!(
            documented,
            radian_arc::worker_args::OWNED_EXEC_ARGS,
            "arc/files.md's `worker.exec_args` row lists the code's owned arguments: {row:?}"
        );
    }

    /// The review stage's own arguments are the ones the `gate.review.exec_args` row names:
    /// the row's Allowed values cell lists exactly `radian_arc::gate::OWNED_EXEC_ARGS`, in
    /// order. The source is the code's one list, so a new or removed owned argument fails
    /// here, and so does a row that names an argument the stage does not own.
    #[test]
    fn the_owned_review_arguments_are_the_config_rows_list() {
        let doc = arc_files_md();
        let table = arc_table(&doc, "arc/files.md", "## `config.toml`");
        let row = config_key_row(&table, "gate.review.exec_args")
            .expect("arc/files.md's config table has a `gate.review.exec_args` row");
        let allowed = row.get(3).expect("the row has an Allowed values cell");
        let documented: Vec<&str> = allowed
            .split('`')
            .skip(1)
            .step_by(2)
            .filter(|token| token.starts_with('-'))
            .collect();
        assert_eq!(
            documented,
            radian_arc::gate::OWNED_EXEC_ARGS,
            "arc/files.md's `gate.review.exec_args` row lists the stage's owned arguments: {row:?}"
        );
    }

    /// `worker.rework_session`'s values are one closed list in the code, and the config table's
    /// row names exactly them, in order. A value added to the list fails here until the row
    /// lists it, and so does a row that names a value the parser refuses.
    #[test]
    fn every_rework_session_value_appears_in_arc_files_md() {
        let doc = arc_files_md();
        let table = arc_table(&doc, "arc/files.md", "## `config.toml`");
        let row = config_key_row(&table, "worker.rework_session")
            .expect("arc/files.md's config table has a `worker.rework_session` row");
        let allowed = row.get(3).expect("the row has an Allowed values cell");
        let documented: Vec<&str> = allowed.split('`').skip(1).step_by(2).collect();
        let code: Vec<&str> = radian_arc::store::config::REWORK_SESSIONS
            .iter()
            .map(|value| value.as_str())
            .collect();
        assert_eq!(
            documented, code,
            "arc/files.md's `worker.rework_session` row lists the code's closed list: {row:?}"
        );
    }

    /// Every environment variable the wrapper adds for a command worker appears in the
    /// environment table under `## Another agent as the worker` on `arc/worker.md`. The source
    /// is `radian_arc::work::WORKER_ENV`, so a new variable fails here until the manual names
    /// it (ADR 0048 decision 7).
    #[test]
    fn every_command_worker_variable_appears_in_arc_worker_md() {
        let (_, doc) = radian_coding::docs::lookup_topic("arc/worker")
            .expect("arc/worker.md is a bundled page");
        let table = arc_table(doc, "arc/worker.md", "## Another agent as the worker");
        for name in radian_arc::work::WORKER_ENV {
            let found = table
                .iter()
                .any(|row| row.iter().any(|cell| cell.contains(&format!("`{name}`"))));
            assert!(
                found,
                "arc/worker.md's command-worker table has no row for `{name}`"
            );
        }
    }

    /// Every attempt failure class appears in the Class cell of the table under
    /// `## Failures, retries and the budget` on `arc/gate.md`. The source is
    /// `radian_arc::attempt::FAILURE_CLASSES`.
    #[test]
    fn every_attempt_failure_class_appears_in_arc_gate_md() {
        let doc = arc_gate_md();
        let table = arc_table(&doc, "arc/gate.md", "## Failures, retries and the budget");
        for class in radian_arc::attempt::FAILURE_CLASSES {
            let found = table
                .iter()
                .any(|row| row.get(1).is_some_and(|cell| cell.contains(class)));
            assert!(
                found,
                "the failure class `{class}` is missing from the failure table"
            );
        }
    }

    /// A `reverify` attempt records its own class; the manual's failure table names it. The
    /// source is [`radian_arc::store::attempt_ops::REVERIFY_CLASS`].
    #[test]
    fn the_reverify_class_appears_in_arc_gate_md() {
        let doc = arc_gate_md();
        let table = arc_table(&doc, "arc/gate.md", "## Failures, retries and the budget");
        let class = radian_arc::store::attempt_ops::REVERIFY_CLASS;
        assert!(
            table
                .iter()
                .any(|row| row.get(1).is_some_and(|cell| cell.contains(class))),
            "the attempt class `{class}` is missing from the failure table"
        );
    }

    /// The gate record's statuses are one closed list in the code, in order: both the gate page
    /// and the file page name exactly them, in that order, in the sentence that says what a
    /// record's `status` is. The source is `radian_arc::gate::background::STATUSES`, which the
    /// parser refuses a value outside. A status added to the list fails here until both pages
    /// list it.
    #[test]
    fn every_gate_record_status_appears_in_the_arc_pages() {
        let statuses = radian_arc::gate::background::STATUSES;
        assert!(!statuses.is_empty(), "the status list is empty");
        let mut listed = String::new();
        for (index, status) in statuses.iter().enumerate() {
            if index > 0 {
                listed.push_str(if index + 1 == statuses.len() {
                    " and "
                } else {
                    ", "
                });
            }
            listed.push_str(&format!("`{status}`"));
        }
        let sentence = format!("`status` is one of {listed}");
        for (name, doc) in [
            ("arc/gate.md", arc_gate_md()),
            ("arc/files.md", arc_files_md()),
        ] {
            assert!(doc.contains(&sentence), "{name} must say {sentence}");
        }
    }

    /// The plan priorities are one closed list in the code, the same list the `priority`
    /// argument's help shows: the plan fields table's `priority` row lists every value and its
    /// default, the `radian arc priority` section of `arc/index.md` lists every value, and the
    /// parser's possible values are exactly the list. A priority added to the list fails here
    /// until both pages and the parser have it.
    #[test]
    fn every_plan_priority_appears_in_the_arc_pages() {
        let fields = arc_files_md();
        let plan_fields = arc_table(&fields, "arc/files.md", "### Plan fields");
        let row = row_with_first_cell(&plan_fields, "`priority`")
            .expect("the plan fields table has a `priority` row");
        let allowed = row
            .get(3)
            .expect("the `priority` row has an Allowed values cell");
        for priority in radian_arc::store::PLAN_PRIORITIES {
            assert!(
                allowed.contains(priority),
                "the `priority` row misses `{priority}`: {row:?}"
            );
        }
        assert!(
            allowed.contains(radian_arc::store::DEFAULT_PRIORITY),
            "the `priority` row must name its default {}: {row:?}",
            radian_arc::store::DEFAULT_PRIORITY
        );

        let commands = arc_md();
        assert!(
            commands.contains(&format!(
                "reads as `{}`",
                radian_arc::store::DEFAULT_PRIORITY
            )),
            "arc/index.md must say a plan without the field reads as `{}`",
            radian_arc::store::DEFAULT_PRIORITY
        );
        let tables = arc_tables(&commands, "arc/index.md", "## `radian arc priority");
        for priority in radian_arc::store::PLAN_PRIORITIES {
            assert!(
                tables
                    .iter()
                    .flatten()
                    .any(|row| row.iter().any(|cell| cell.contains(priority))),
                "arc/index.md's `radian arc priority` section misses `{priority}`"
            );
        }

        let arc = arc_subcommand();
        let priority = arc
            .get_subcommands()
            .find(|sub| sub.get_name() == "priority")
            .expect("the parser has `arc priority`");
        let arg = priority
            .get_arguments()
            .find(|arg| arg.get_id() == "priority")
            .expect("`priority` takes the value");
        let shown: Vec<String> = arg
            .get_possible_values()
            .iter()
            .map(|value| value.get_name().to_owned())
            .collect();
        assert_eq!(
            shown,
            radian_arc::store::PLAN_PRIORITIES,
            "the parser's help lists the code's one priority list"
        );
    }

    /// Every plan status, auto level, stage, task state, and each state's phases appear in
    /// the table that owns them on the page that holds it: the plan fields table's `status`
    /// row (`arc/files.md`), the level table (`arc/index.md`), the config table's `stages` row
    /// (`arc/files.md`), and the states table (`arc/files.md`). A mention elsewhere on a
    /// page does not satisfy a check.
    #[test]
    fn every_arc_value_set_appears_in_the_arc_pages() {
        let commands = arc_md();
        let files = arc_files_md();
        let plan_fields = arc_table(&files, "arc/files.md", "### Plan fields");
        let status_row = row_with_first_cell(&plan_fields, "`status`")
            .expect("the plan fields table has a `status` row");
        let allowed = status_row
            .get(3)
            .expect("the plan fields table has an Allowed values cell");
        for status in radian_arc::store::PLAN_STATUSES {
            assert!(
                allowed.contains(status),
                "the `status` row misses `{status}`: {status_row:?}"
            );
        }
        let levels = arc_table(&commands, "arc/index.md", "### The levels");
        for level in radian_arc::store::AUTO_LEVELS {
            assert!(
                levels
                    .iter()
                    .any(|row| first_cell(row) == format!("`{level}`")),
                "the level table has no `{level}` row"
            );
        }
        let config = arc_table(&files, "arc/files.md", "## `config.toml`");
        let stages_row = row_with_first_cell(&config, "`gate.profiles.<name>.stages`")
            .expect("the config table has a `gate.profiles.<name>.stages` row");
        let allowed = stages_row
            .get(3)
            .expect("the stages row has an Allowed values cell");
        for stage in radian_arc::store::STAGE_NAMES {
            assert!(
                allowed.contains(stage),
                "the stages row misses `{stage}`: {stages_row:?}"
            );
        }
        let states = arc_table(&files, "arc/files.md", "### States and phases");
        for state in radian_arc::store::TASK_STATES {
            let row = row_with_first_cell(&states, &format!("`{state}`"))
                .unwrap_or_else(|| panic!("the states table has no `{state}` row"));
            let phases = radian_arc::store::TASK_PHASES
                .iter()
                .find(|(name, _)| name == state)
                .map(|(_, phases)| *phases)
                .expect("every state has a phase list");
            for phase in phases {
                assert!(
                    row.get(1).is_some_and(|cell| cell.contains(phase)),
                    "state `{state}` row misses phase `{phase}`: {row:?}"
                );
            }
        }
    }

    /// Every front-matter field the writer puts in a plan or a task file has a row in that
    /// file's own table under `### Plan fields` / `### Task fields` on `arc/files.md`. The
    /// source is the
    /// writer: a renamed or added field fails here, and a task field cannot be satisfied by
    /// a plan row.
    #[test]
    fn every_plan_and_task_field_appears_in_arc_files_md() {
        use radian_arc::store::model::{PlanFile, TaskFile, TaskState};
        let doc = arc_files_md();
        let plan = PlanFile {
            schema_version: 1,
            id: "0001-a".to_owned(),
            title: "A plan".to_owned(),
            status: "draft".to_owned(),
            auto: "off".to_owned(),
            priority: Some("P3".to_owned()),
            max_cost_usd: Some(5.0),
            max_wall_time_secs: Some(7200),
            tasks: vec!["0001-b".to_owned()],
            body: String::new(),
        };
        let task = TaskFile {
            schema_version: 1,
            id: "0001-b".to_owned(),
            plan: "0001-a".to_owned(),
            title: "A task".to_owned(),
            state: TaskState::Todo,
            phase: "ready".to_owned(),
            after: vec!["0002-c".to_owned()],
            flow: Some("default".to_owned()),
            supersedes: Some("0003-d".to_owned()),
            attempt: 0,
            attempt_id: Some("0001-b-1".to_owned()),
            token: Some("a-token".to_owned()),
            cost_usd: Some(0.5),
            total_cost_usd: Some(0.75),
            cost_unknown: true,
            auto_rework: 2,
            retry_count: 1,
            allow_noop: Some(false),
            followup_pending: true,
            extra_stages: vec!["mutation".to_owned()],
            max_attempts: Some(2),
            max_cost_usd: Some(1.0),
            max_wall_time_secs: Some(600),
            stall_timeout_secs: Some(300),
            base_sha: Some("a".repeat(40)),
            head_sha: Some("b".repeat(40)),
            verified_sha: Some("c".repeat(40)),
            verdict_set_digest: Some("d".repeat(64)),
            retry_at: Some("2026-01-01T00:00:00Z".to_owned()),
            body: String::new(),
        };
        let plan_table = arc_table(&doc, "arc/files.md", "### Plan fields");
        for key in front_matter_keys(&plan.to_text()) {
            assert!(
                row_with_first_cell(&plan_table, &format!("`{key}`")).is_some(),
                "plan field `{key}` has no row in arc/files.md's plan fields table"
            );
        }
        let task_table = arc_table(&doc, "arc/files.md", "### Task fields");
        for key in front_matter_keys(&task.to_text()) {
            assert!(
                row_with_first_cell(&task_table, &format!("`{key}`")).is_some(),
                "task field `{key}` has no row in arc/files.md's task fields table"
            );
        }
    }

    /// Every MCP tool of `radian arc mcp` and every parameter of every tool appears in the
    /// table under `### The tools` on `arc/mcp.md`, with the required or optional word the
    /// schema declares.
    /// The source is `radian_arc::mcp::TOOLS`, the same list `tools/list` answers from, so a
    /// renamed tool or parameter fails here and a tool added without a page entry fails too.
    /// Mutation: drop one tool from the page's table → this fails naming it.
    #[test]
    fn every_mcp_tool_appears_in_arc_mcp_md() {
        let doc = arc_mcp_md();
        let table = arc_table(&doc, "arc/mcp.md", "### The tools");
        assert!(!radian_arc::mcp::TOOLS.is_empty(), "the server has tools");
        for tool in radian_arc::mcp::TOOLS {
            let row =
                row_with_first_cell(&table, &format!("`{}`", tool.name)).unwrap_or_else(|| {
                    panic!("arc/mcp.md's tools table has no row for `{}`", tool.name)
                });
            let parameters = row
                .get(1)
                .unwrap_or_else(|| panic!("`{}`'s row has no Parameters cell", tool.name));
            for (name, required) in tool.parameters() {
                let marker = if required { "required" } else { "optional" };
                let at = parameters.find(&format!("`{name}`")).unwrap_or_else(|| {
                    panic!(
                        "`{}`'s parameter `{name}` is missing from its row: {row:?}",
                        tool.name
                    )
                });
                let rest = parameters[at + name.len() + 2..]
                    .split(')')
                    .next()
                    .unwrap_or("");
                assert!(
                    rest.contains(marker),
                    "`{}`'s `{name}` must be marked `{marker}` in its row: {row:?}",
                    tool.name
                );
            }
        }
    }

    /// Every phase and plan field the channel reports, the notification method, the capability
    /// and the version ceiling appear on `arc/mcp.md`, with the polling interval. The source is
    /// `radian_arc::mcp`, so a phase or field added to or dropped from the sent sets fails here.
    /// Mutation: drop a phase from the page → this fails naming it.
    #[test]
    fn every_channel_phase_appears_in_arc_mcp_md() {
        let doc = arc_mcp_md();
        for (what, needle) in [
            ("the notification method", radian_arc::mcp::CHANNEL_METHOD),
            (
                "the channel capability",
                radian_arc::mcp::CHANNEL_CAPABILITY,
            ),
            (
                "the permission capability it does not declare",
                radian_arc::mcp::CHANNEL_PERMISSION_CAPABILITY,
            ),
            (
                "the version ceiling",
                radian_arc::mcp::CHANNEL_MAX_VERSION.as_str(),
            ),
        ] {
            assert!(doc.contains(needle), "arc/mcp.md does not name {what}");
        }
        assert!(
            doc.contains(&format!(
                "every {} seconds",
                radian_arc::mcp::CHANNEL_INTERVAL_SECS
            )),
            "arc/mcp.md does not name the polling interval"
        );
        assert!(
            !radian_arc::mcp::CHANNEL_PHASES.is_empty(),
            "the set is sent"
        );
        for phase in radian_arc::mcp::CHANNEL_PHASES {
            assert!(
                doc.contains(&format!("`{phase}`")),
                "arc/mcp.md is missing the channel phase `{phase}`"
            );
        }
        for phase in radian_arc::mcp::CHANNEL_READY_FROM {
            assert!(
                doc.contains(&format!("`{phase}`")),
                "arc/mcp.md is missing the phase `{phase}` a move into `todo.ready` comes from"
            );
        }
        for field in radian_arc::mcp::CHANNEL_PLAN_FIELDS {
            assert!(
                doc.contains(&format!("`{field}`")),
                "arc/mcp.md is missing the plan field `{field}`"
            );
        }
    }

    /// The flattened TOML default: `(dotted key, rendered value)` for every non-table leaf.
    fn flatten_toml(prefix: &str, value: &toml::Value, out: &mut Vec<(String, String)>) {
        match value {
            toml::Value::Table(table) => {
                for (key, value) in table {
                    let key = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    flatten_toml(&key, value, out);
                }
            }
            other => out.push((prefix.to_owned(), other.to_string())),
        }
    }

    /// The config table's row for `key`. The page writes a profile key as
    /// `gate.profiles.<name>.x`; the default section's own spelling is accepted too. Only
    /// the first cell counts, so a decoy row with a matching prefix is never read.
    fn config_key_row<'a>(table: &'a DocTable, key: &str) -> Option<&'a Vec<String>> {
        let spellings = [
            format!("`{key}`"),
            format!("`{}`", key.replace(".default.", ".<name>.")),
        ];
        table
            .iter()
            .find(|row| spellings.iter().any(|spelling| first_cell(row) == spelling))
    }

    /// The Default cell of a five-column table row.
    fn config_default_cell(row: &[String]) -> &str {
        row.get(2).map(String::as_str).unwrap_or("")
    }

    /// Parse one TOML value written inside a Default cell. A TOML document needs a key, so
    /// wrap the value in one.
    fn parse_toml_value(text: &str) -> Option<toml::Value> {
        let mut table: toml::Table = toml::from_str(&format!("value = {text}\n")).ok()?;
        table.remove("value")
    }

    /// The first backticked value in a Default cell, parsed as TOML. `not set: no limit`
    /// has no value and returns `None`.
    fn parse_default_cell(cell: &str) -> Option<toml::Value> {
        let start = cell.find('`')?;
        let rest = &cell[start + 1..];
        let end = rest.find('`')?;
        parse_toml_value(&rest[..end])
    }

    /// The front-matter keys of a plan or task file, in file order.
    fn front_matter_keys(text: &str) -> Vec<String> {
        let mut keys = Vec::new();
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some("---"));
        for line in lines {
            if line == "---" {
                break;
            }
            if line.starts_with(' ') || line.starts_with('\t') {
                continue;
            }
            if let Some((key, _)) = line.split_once(':') {
                keys.push(key.trim().to_owned());
            }
        }
        keys
    }
}
