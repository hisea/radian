//! The `radian` binary: one real conversation through `radian-runtime`, rendered full
//! screen.
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
    check_provider_flags, load_mcp_config, parse_headers, validate_session_id,
};
use radian_permissions::{AuditSink, PermissionResolved, runtime_event};
use radian_session::{Index, SessionJournal};
use radian_tui::app::{AUTO_SCROLL_TICK, App, Effect, Input as AppInput, MemoryResult, PickerData};
use radian_tui::clip::{ClipboardMode, osc52};
use radian_tui::events::{InputEvent, ProjectionSink, UiEvent};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event};
use ratatui::crossterm::terminal;

#[derive(Parser, Debug)]
#[command(
    name = "radian",
    version,
    about = "Full-screen TUI for the radian agent runtime"
)]
struct Cli {
    /// Model name sent to the API. Required unless the config file's `model.model` provides
    /// it.
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

    /// Extra request header, repeatable: `--header NAME=VALUE`.
    #[arg(long = "header", value_name = "NAME=VALUE")]
    header: Vec<String>,

    /// Where session files live. Defaults to `RADIAN_SESSION_DIR`, then `~/.radian/sessions`
    /// (`$RADIAN_HOME/sessions` when that is set, ADR 0020). The same rule as `agent-cli`.
    #[arg(long, value_name = "DIR")]
    session_dir: Option<PathBuf>,

    /// A stable session identifier: the session file's name. Generated once per process when
    /// omitted.
    #[arg(long, value_name = "VALUE", conflicts_with = "resume")]
    session_id: Option<String>,

    /// Resume an existing session by id, rendering its history into the transcript. Without a
    /// value it shows the session picker (resuming only: no "new" entry).
    #[arg(long, value_name = "SESSION_ID", num_args = 0..=1, conflicts_with = "list")]
    resume: Option<Option<String>>,

    /// List this workspace's sessions and exit.
    #[arg(long)]
    list: bool,

    /// Show a full-screen picker for this workspace's sessions before opening one (Phase 4):
    /// Up/Down select, type to filter, Tab full-text search, Enter resume, n new, Esc quit.
    /// Conflicts with `--resume`/`--session-id`; it does not create a session file.
    #[arg(long, conflicts_with_all = ["resume", "session_id"])]
    pick: bool,

    /// Expose the built-in file and read-only git tools rooted at this directory
    /// (read/write/patch/list, git_status/git_diff). No shell is provided.
    #[arg(long, value_name = "DIR")]
    workspace: Option<PathBuf>,

    /// A JSON file listing MCP stdio servers to start (`{"servers": [...]}`), the same format
    /// `agent-cli` reads. Under `workspace` their tools ask before running and the
    /// servers themselves are **not** sandboxed (ADR 0012 option B).
    #[arg(long, value_name = "FILE")]
    mcp_config: Option<String>,

    /// The permission tier (ADR 0016): `safe` denies file modification, `workspace`
    /// allows writes that resolve inside the workspace, `full` also allows MCP. Defaults to
    /// the config file's `permissions.profile`, then `workspace`.
    #[arg(long, value_name = "TIER")]
    profile: Option<String>,

    /// How `ask` is resolved (ADR 0012's ApprovalPolicy): `on-ask` asks when a rule says so,
    /// `always` asks for every call a rule does not deny, `never` turns `ask` into a denial,
    /// `auto` answers every `ask` with "allow" (ADR 0026 D9). Omit to fall back to the
    /// config's `[permissions] approval`, then the session's journaled state, then `on-ask`.
    #[arg(long, value_name = "POLICY")]
    approval: Option<String>,

    /// Approve every tool call without asking, like `agent --yes`. Not "allow everything":
    /// `deny` rules still deny and the sandbox still applies; only the question is removed.
    #[arg(long)]
    yes: bool,

    /// The per-turn model call limit. The core's default (`DEFAULT_MAX_MODEL_CALLS`, 32) is a
    /// fuse for unattended runs; an interactive coding session reads many files per turn and
    /// a human can cancel, so the TUI defaults much higher. It is still a fuse: a loop of
    /// small calls is not stopped by the context budget (which only triggers compaction).
    #[arg(long, value_name = "N", default_value_t = TUI_DEFAULT_MAX_MODEL_CALLS)]
    max_model_calls: u32,

    /// Allow a shell command prefix without asking, repeatable (e.g. `--allow-shell-command
    /// "git status"`). Only a *simple* command matches: any shell metacharacter keeps the
    /// call at `ask`. Write these at the subcommand level, never at a bare program name.
    #[arg(long = "allow-shell-command", value_name = "PREFIX")]
    allow_shell_command: Vec<String>,

    /// The model's context window in tokens (ADR 0015). Omitted, the models.dev catalog is
    /// consulted for `(provider, model)` (ADR 0018); an explicit value always wins. With
    /// neither, the budget is off and the status bar says so.
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

    #[arg(long)]
    system: Option<String>,

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

    /// Test hook, **kept** as a diagnostic: append
    /// `<point> <unix-nanos>` to this file at each step of the terminal-gone exit path. It
    /// tells "the exit path ran but slowly" from "the POLLHUP path never fired" (and, with
    /// the main-loop fallback, shows the fallback firing: a trace that lacks `pollhup` but has
    /// `loop-done`). Inert when the flag is absent: no file, no output, and the input thread's
    /// per-site counters are not even touched (`exit_trace_once` returns on an unset `OnceLock`).
    #[arg(long, hide = true, value_name = "FILE")]
    exit_trace: Option<PathBuf>,
}

/// The TUI's per-turn model call limit when `--max-model-calls` is not given (decision
/// 2026-10-12, dogfooding finding D6).
const TUI_DEFAULT_MAX_MODEL_CALLS: u32 = 1000;

/// How many facts one loop turn processes before it draws. Bounded so a producer that keeps the
/// channel busy cannot starve the frame (ADR 0023 U2's draw fairness): the fairness pty test
/// floods 20 000 facts and still sees at least 100 frames. 64 keeps a turn's work near the
/// pre-refactor 50 ms tick (~50 deltas) while staying far below a frame budget.
const MAX_FACTS_PER_TURN: usize = 64;

/// How many effect rounds one turn may take. An effect's result is another event, and a result
/// can ask for one more effect (`@` expansion → send), so the chain is short; the bound turns a
/// bug into an error instead of a hang.
const MAX_EFFECT_ROUNDS: usize = 8;

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

fn main() -> ExitCode {
    radian_session::trace::start();
    radian_tui::terminal::install_panic_hook();
    let cli = Cli::parse();
    if let Some(path) = &cli.exit_trace {
        let _ = EXIT_TRACE.set(path.clone());
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
        Ok(Err(message)) => {
            print_exit_line(&message);
            ExitCode::from(1)
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

/// Prints the exit reason on the normal screen. `eprintln!` panics when the terminal is
/// already gone (a closed pty makes the write fail), which would turn a clean exit into 101;
/// a failure to print the line is not a failure to exit.
fn print_exit_line(message: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "radian: {message}");
}

fn run(
    cli: Cli,
    runtime: &tokio::runtime::Runtime,
    rx: Receiver<UiEvent>,
    tx: Sender<UiEvent>,
) -> Result<Option<String>, String> {
    mark("run-start");
    if cli.list {
        return match list_sessions(&cli) {
            Ok(()) | Err(ListError::BrokenPipe) => Ok(None),
            Err(ListError::Failed(message)) => Err(message),
        };
    }
    // ADR 0013 decision 4: diagnostics live in the user cache directory, and cleanup runs at
    // startup. A failure is one line, never a fault.
    radian_host::clean_startup();
    let dir = session_dir(&cli)?;
    let workspace_root = coding_root(&cli)?;
    let workspace_path = radian_host::resolve_workspace(Some(&workspace_root.path))?;
    // ADR 0020 decision 5: never move a user's data; print a copy-pasteable command instead.
    if cli.session_dir.is_none()
        && std::env::var("RADIAN_SESSION_DIR").map_or(true, |dir| dir.is_empty())
        && let Some(hint) = radian_host::legacy_sessions_hint()
    {
        eprintln!("{hint}");
    }
    // ADR 0008: the config file provides defaults; every CLI flag wins. A refused file fails
    // startup before any session file exists.
    let config =
        radian_host::load_config(cli.config.as_deref()).map_err(|error| error.to_string())?;
    if config.read_only {
        eprintln!(
            "[config] schema_version {} is newer than the supported v{}; reading it read-only",
            config.schema_version.unwrap_or(0),
            radian_host::SUPPORTED_SCHEMA_VERSION
        );
    }
    // A keymap conflict refuses startup before the terminal is entered.
    if let Err(error) = radian_tui::keys::Keymap::from_config(&config.tui.keybindings) {
        return Err(format!("config: {error}"));
    }
    // ADR 0026 D8: on a resume the journal's last model change outranks the config file (the
    // command line still outranks both). `--resume`'s id is known by here (`dir` too).
    // `--resume` without a value (and `--pick`) are decided here, before the model block
    // below: `resume` is what the journal route is read from (review M1).
    let resume_only = matches!(cli.resume, Some(None));
    let mut resume = cli.resume.clone().flatten();
    let startup_picker = if cli.pick || resume_only {
        radian_session::trace::mark("picker/prepare-start");
        let workspace = workspace_path.to_string_lossy().into_owned();
        prepare_picker(&dir, &workspace)?.map(|picker| {
            if resume_only {
                picker.without_new()
            } else {
                picker
            }
        })
    } else {
        None
    };
    radian_session::trace::mark("picker/prepared");
    // Nothing to resume is a refusal, not a new session: `--resume` without a value never
    // creates a file.
    if resume_only && startup_picker.is_none() {
        return Err(
            "no sessions for this workspace to resume; run without --resume to start one"
                .to_string(),
        );
    }
    let pick_notice = cli.pick && startup_picker.is_none();

    // ADR 0020 decision 3 / ADR 0013: create the session directory owner-only, or warn once
    // if it already exists and is group/world-readable. Done **before** the alternate screen
    // is entered: a raw `eprintln!` inside it writes cells the renderer's diff then skips.
    let _ = radian_host::ensure_private_dir(&dir);
    // The privacy warning about a world-readable session directory is a startup diagnostic:
    // printed here, on the normal screen. (Inside the alternate screen it would corrupt the
    // display, and `/sessions` used to print it from there; `warn_if_wide` is once per path,
    // so this is the only place it appears.)
    radian_session::warn_if_wide(&dir);
    // ADR 0008: the `[config]` line and its warnings are startup diagnostics and belong on
    // the normal screen, **before** the alternate screen is entered. Printed inside it, a
    // stray `eprintln!` writes cells the renderer's diff then skips (both buffers see a
    // space), so the raw text stays on screen and corrupts the first row.
    // The session id is not known yet when `--pick` runs (the picker chooses it), so the app
    // starts with an empty one and `run` sets the real id after the choice.
    // The model is resolved **after** the picker (review M1), so the app starts with a
    // placeholder name and `run` sets the real one below.
    let mut app = App::new(String::new(), String::new());
    // `Tab` (the `complete` action) completes `@` file references against the workspace.
    app.set_workspace(workspace_path.clone());
    let mut config_warnings = app
        .apply_config(&config, radian_tui::theme::Theme::no_color_env())
        .map_err(|error| format!("config: {error}"))?;
    // ADR 0016: `[sandbox] allow_unix_sockets`, `~`-expanded and canonicalized
    // before the sandbox is built. An entry that does not resolve is skipped and named here.
    let (allow_unix_sockets, socket_warnings) = radian_host::resolve_allow_unix_sockets(
        &config.sandbox.allow_unix_sockets,
        radian_host::user_home().as_deref(),
    );
    config_warnings.extend(socket_warnings);
    match &config.source {
        Some(path) => eprintln!("[config] {}", path.display()),
        None => eprintln!("[config] none"),
    }
    for warning in &config_warnings {
        eprintln!("[config] {warning}");
    }

    // The terminal is entered here **only** for `--pick`: the picker needs it before the
    // session exists. A plain run enters it later, after every fallible startup check and the
    // session itself, so a refusal leaves no alternate screen behind (dogfooding D2).
    let mouse = config.tui.mouse.unwrap_or(true);
    let mut startup_terminal = None;
    if startup_picker.is_some() {
        startup_terminal = Some(enter_terminal(mouse)?);
        spawn_input_thread(tx.clone());
    }
    // ADR 0023 U4 / plan B: `--pick` is the session picker **overlay**, not a separate
    // terminal loop. The process enters the terminal once; the loop runs a pre-session phase
    // (the picker only) until the user chooses, then the ordinary session phase takes over in
    // the same terminal. No session file, journal, driver or MCP connection exists until the
    // choice is made (the session id and any `{session_id}` header are resolved after it).
    if let Some(picker) = startup_picker {
        radian_session::trace::mark("picker/before-first-frame");
        let (guard, terminal) = startup_terminal
            .as_mut()
            .expect("the terminal was entered for the picker");
        app.start_picking(picker);
        let signal = ui_loop(
            terminal,
            &mut app,
            &rx,
            &tx,
            Phase::Picking,
            // The picking phase has no resolved model yet (review M1): the route is only a
            // fallback for the `/thinking` clamp, which the picker overlay cannot reach.
            (
                cli.provider.clone().unwrap_or_default(),
                cli.model.clone().unwrap_or_default(),
            ),
            &config.providers,
            config.tui.theme.clone(),
            cli.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            // The picker phase has no session and no sandbox yet.
            None,
            None,
        )?;
        if let Some(signal) = signal {
            // A signal during the pre-session phase leaves exactly like the session phase —
            // restore the terminal, exit 128 + signal, print the reason — but there is no
            // session to name, so no `--resume` hint. No session file has been created.
            radian_tui::signals::confirm_exit();
            guard.restore();
            return Ok(Some(format!(
                "received {}",
                radian_tui::signals::name(signal)
            )));
        }
        app.finish_picking();
        match app.take_picker_outcome() {
            Some(radian_tui::picker::Outcome::Resume(id)) => resume = Some(id),
            Some(radian_tui::picker::Outcome::New) => {}
            // `Quit`, Ctrl-C, or the terminal going away before any session: nothing was
            // created, so leave through the ordinary clean-exit path.
            _ => {
                radian_tui::signals::confirm_exit();
                guard.restore();
                return Ok(None);
            }
        }
    }

    // ADR 0026 D8 + review M1: this runs **after** the picker, so `--pick` and a valueless
    // `--resume` have already decided `resume` and the journal's last model change is what
    // resolves the model, provider, credential, route, window and budget — one path, not a
    // config-based guess that the driver's replay then contradicts. The command line still
    // outranks the journal, which outranks the config.
    // `resume` is the **resolved** id (the picker set it for `--pick`/a valueless `--resume`),
    // not `cli.resume`: reading the journal needs the id the session will actually open with.
    let resumed_route = match resume.as_deref() {
        Some(id) => radian_host::resumed_model_route(&dir, id),
        None => None,
    };
    let model_name = cli
        .model
        .clone()
        .or_else(|| resumed_route.as_ref().map(|route| route.model.clone()))
        .or_else(|| config.model.model.clone())
        .ok_or_else(|| {
            "no model: pass --model, or set model.model in the config file".to_string()
        })?;
    let profile_name = cli
        .profile
        .clone()
        .or_else(|| config.permissions.profile.clone())
        .unwrap_or_else(|| "workspace".to_string());
    // ADR 0026 D0/D10: `--provider`/config `model.provider` names a provider — a built-in
    // descriptor or a named one (`[providers.<name>]` in the config, or the auth.json
    // entry's `env`). The generic `openai-compatible` descriptor is the default (the pre-D0
    // behaviour); the model is never defaulted. The key is looked up under the **name**
    // (D10), so every custom endpoint has its own slot.
    // D8: the command line wins. An explicit `--base-url` is the generic provider's dial
    // (D0), so it counts as the user choosing generic; otherwise the journal's route outranks
    // the config (review M1: this runs after the picker, so the id is known).
    let provider_name = if cli.base_url.is_some() {
        cli.provider
            .clone()
            .unwrap_or_else(|| radian_host::GENERIC_ID.to_owned())
    } else {
        cli.provider
            .clone()
            .or_else(|| resumed_route.as_ref().map(|route| route.provider.clone()))
            .or_else(|| config.model.provider.clone())
            .unwrap_or_else(|| radian_host::GENERIC_ID.to_owned())
    };
    let auth_store = radian_host::AuthStore::open().map_err(|e| e.to_string())?;
    let (provider_ref, provider_warning) = radian_host::resolve_provider(
        &provider_name,
        &config.providers,
        auth_store.entry_env(&provider_name).as_ref(),
    )?;
    if let Some(warning) = &provider_warning {
        // A display conflict, not an error: the startup line says which side won.
        eprintln!("[provider] {warning}");
    }
    let key_id = provider_ref.key_id().to_owned();
    let provider = provider_ref.protocol_for(&model_name).adapter();
    // ADR 0026 D11: the unified levels, with the legacy Anthropic spellings accepted for one
    // version (a warning goes to stderr when one is used).
    let (thinking, thinking_budget, legacy_thinking) = match cli.thinking.as_deref() {
        Some(raw) => {
            let (level, budget, legacy) = radian_host::parse_thinking_level(raw)?;
            (level.to_owned(), budget, legacy)
        }
        None => (radian_host::DEFAULT_THINKING_LEVEL.to_owned(), None, false),
    };
    // The footer's `reasoning` item shows the level that actually takes effect (ADR 0026
    // D11): the requested level clamped against this route's map, so a model that does not
    // support `medium` shows `high` rather than the config value. `thinking` is moved into the
    // route below.
    let thinking_for_footer = radian_tui::command::effective_thinking_level(
        &provider_ref,
        &model_name,
        &thinking,
        cli.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
    );
    if legacy_thinking {
        eprintln!(
            "radian: [thinking] the legacy `--thinking {}` spelling is deprecated; use one of {}",
            cli.thinking.as_deref().unwrap_or_default(),
            radian_host::THINKING_LEVELS.join("|")
        );
    }
    // `--max-tokens` is Anthropic-only; under openai-compatible it would be ignored, so this is
    // an error before the session file exists (dogfooding D15). `--thinking` left this check in
    // D11: the unified levels ride both wire protocols.
    check_provider_flags(provider, Thinking::Off, cli.max_tokens)?;
    // The generic descriptor's endpoint; a non-generic descriptor refuses it (`resolve_route`).
    // The generic provider's endpoint override (D0): refused for any other provider, so it is
    // dropped here rather than passed to `resolve_route_for`, which would error.
    let base_url_override = cli
        .base_url
        .clone()
        .or_else(|| config.model.base_url.clone())
        .filter(|_| provider_ref.takes_base_url_override());
    // The endpoint this run would talk to, before the session id: used only to allow a local
    // server to run without a key (a loopback mock needs no credential). `--base-url` only
    // pre-fills the probe for the generic provider (a named one has a fixed endpoint).
    let probe_base_url = if provider_ref.takes_base_url_override() {
        base_url_override.clone()
    } else {
        None
    }
    .unwrap_or_else(|| provider_ref.base_url_for(&model_name, None));
    // ADR 0026 D2: the key comes only from `auth.json`. A missing entry is a clear refusal with
    // the fix (run `/login <provider>` — D3, in this slice), unless the endpoint is local.
    let credential = match radian_host::resolve_key(&key_id).map_err(|e| e.to_string())? {
        Some(credential) => Some(credential),
        None if is_local_base_url(&probe_base_url) => None,
        None => {
            return Err(
                match radian_host::resolve_note(&key_id).map_err(|e| e.to_string())? {
                    Some(note) => note,
                    None => format!(
                        "no credential for `{key_id}`: run `agent login {key_id}`, or \
                         `/login {key_id}` in the TUI; refusing to continue because {} is not a \
                         local address",
                        radian_host::redact_base_url(&probe_base_url)
                    ),
                },
            );
        }
    };
    // The config's headers come first, so a `--header` of the same name wins.
    let mut header_args: Vec<String> = config
        .model
        .headers
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect();
    header_args.extend(cli.header.iter().cloned());
    // `R` comes from the request's own output setting: Anthropic sends `max_tokens`,
    // OpenAI-compatible sends none.
    // The app was built before the model was resolved (review M1): the status bar's model name
    // is set here, once, from the effective route.
    app.model_name = model_name.clone();
    let request_max_output = match provider {
        Provider::Anthropic => Some(cli.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS)),
        Provider::OpenAiCompatible => None,
    };
    // The window's base URL: the provider's endpoint (for a named provider, its declaration).
    let catalog_base_url = provider_ref.base_url_for(&model_name, None);
    // The catalog groups by provider id; the generic descriptor is not one, so its provider
    // is derived from the base URL hostname. A named provider is its own group only when its
    // name is a catalog provider; otherwise the hostname rule applies (the fallback below).
    let catalog_provider = cli
        .catalog_provider
        .clone()
        .or_else(|| match &provider_ref {
            radian_host::ProviderRef::BuiltIn(descriptor) => {
                (descriptor.id != radian_host::GENERIC_ID).then(|| descriptor.id.to_string())
            }
            radian_host::ProviderRef::Named(named) => Some(named.name.clone()),
        });
    let window = runtime.block_on(radian_host::resolve(&radian_host::WindowQuery::production(
        cli.context_window.or(config.model.context_window),
        catalog_provider,
        &catalog_base_url,
        model_name.clone(),
        request_max_output,
        cli.offline_catalog,
    )));
    // ADR 0018: `C ≤ R` refuses an explicit `--context-window` and only warns for a catalog
    // window (the budget is then off).
    match window.startup_check() {
        radian_host::WindowCheck::Ok => {}
        radian_host::WindowCheck::Refuse(message) => return Err(message),
        radian_host::WindowCheck::Warn(message) => eprintln!("{message}"),
    }
    if let Some(warning) = radian_host::output_ceiling_warning(request_max_output, window.output) {
        eprintln!("{warning}");
    }

    // `--resume` with no value: the picker in "resume only" mode.
    // `--pick` (and `--resume` without a value): load the picker's rows **before** the terminal
    // is entered. `open_index` can warn about a wide session directory, and that warning must be
    // on the normal screen. The picker owns its data; the phase itself runs after the terminal
    // (ADR 0023 U4).
    let session_id = match (&resume, &cli.session_id) {
        (Some(id), _) | (None, Some(id)) => id.clone(),
        (None, None) => radian_host::generate_session_id(),
    };
    validate_session_id(&session_id)?;
    app.session_id = session_id.clone();
    let user_headers = parse_headers(&header_args, &session_id)?;
    // Header values and **every** key in `auth.json` must not reach the panic log or an export
    // (dogfooding D4; ADR 0013 T4). The startup provider's key alone is not enough: a session
    // can mention another login, and `/model` can switch to one. `resolved_keys` reads and
    // resolves them all, the same list the CLI's export uses.
    let mut redactions: Vec<String> = radian_host::resolved_keys();
    redactions.extend(user_headers.iter().map(|(_, value)| value.clone()));
    redactions.retain(|secret| !secret.is_empty());
    // Shared and append-only: `/login` adds a key mid-session, `/logout` never removes one.
    let secrets: radian_tui::terminal::SharedSecrets =
        std::sync::Arc::new(std::sync::RwLock::new(redactions));
    // ADR 0026 D0/D10: the provider decides the adapter, base URL and headers.
    // `resolve_route_for` refuses `--base-url`/`--header` for everything but the generic
    // descriptor (a named provider's endpoint is its declaration).
    let route = radian_host::resolve_route_for(
        &provider_ref,
        &model_name,
        base_url_override.as_deref(),
        credential
            .as_ref()
            .and_then(|credential| credential.env.get(radian_host::RADIAN_BASE_URL))
            .map(String::as_str),
        &user_headers,
        &session_id,
    )?;
    // D1/D2: every check that can fail runs **before** the session file is created. The
    // workspace is validated first, so a typo is a workspace error and never reaches the
    // sandbox probe (dogfooding D1). `radian-host` owns the rule so both hosts share it.
    let (thinking_of, reasoning_effort) = radian_host::resolve_thinking_ref(
        &provider_ref,
        &model_name,
        &thinking,
        thinking_budget,
        route.adapter(),
        Some(cli.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS)),
    );
    let initial = build_model(ModelArgs {
        provider: route.adapter(),
        base_url: route.base_url.clone(),
        model: model_name.clone(),
        api_key: credential.as_ref().map(|credential| credential.key.clone()),
        headers: route.headers.clone(),
        // The TUI has no `--include-usage` flag; the adapter's default policy applies.
        include_usage: None,
        max_tokens: cli.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        thinking: thinking_of,
        reasoning_effort,
    })?;
    // ADR 0026 D5: the model is switchable. The slot is shared with the driver (open_session
    // wires it), so a `/model` change reaches the next request without touching the key.
    let config_slot = radian_runtime::ConfigSlot::new(serde_json::Value::Null);
    let model = radian_host::SwitchableModel::new(
        initial,
        radian_host::ModelRoute::new(&provider_name, &model_name),
        std::sync::Arc::clone(&config_slot),
        radian_host::RouteContext {
            providers: config.providers.clone(),
            session_id: session_id.clone(),
            user_headers: user_headers.clone(),
            include_usage: None,
            max_tokens: cli.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            thinking,
            thinking_budget,
        },
    );
    // ADR 0026 D7: a switch recomputes `B` from the warm local catalog, never the network.
    let window_query = radian_host::WindowQuery::production(
        cli.context_window.or(config.model.context_window),
        (provider_name != radian_host::GENERIC_ID).then(|| provider_name.clone()),
        catalog_base_url.clone(),
        model_name.clone(),
        request_max_output,
        true,
    );
    let budget_for = std::sync::Arc::new(move |provider: &str, switch_model: &str| {
        // The switched route carries its own provider id, so the resolver uses it directly
        // instead of deriving one from the startup base URL.
        radian_host::resolve_offline(&radian_host::WindowQuery {
            catalog_provider: Some(provider.to_owned()),
            model: switch_model.to_owned(),
            ..window_query.clone()
        })
        .budget
    }) as radian_runtime::BudgetFor;
    // The permission tier and the sandbox. For an enforced tier a probe failure is an error
    // here, never a fallback to an unsandboxed run (ADR 0012 decision 2). Still before
    // the session file exists, so a refusal leaves nothing behind (dogfooding D2).
    let tier = radian_tui::profile::parse_tier(&profile_name)?;
    // ADR 0020 decision 3: the sandbox denies read on the **actual** sessions directory and
    // the memory directory. The directory was created owner-only before the terminal was
    // entered (`ensure_private_dir` above); Linux's bwrap can only mask paths that exist.
    let memory_root = radian_host::memory_dir();
    let mut sandbox_deny_read = vec![dir.clone()];
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
    let sandbox = radian_tui::profile::build_sandbox(
        tier,
        &workspace_path,
        &sandbox_deny_read,
        &allow_unix_sockets,
    )?;
    // ADR 0033: the user's `!`/`!!` commands run in the **same** sandbox the coding tools use
    // (including ADR 0016's Unix-socket rules). `Sandbox` is cheap to clone; `None`
    // (the `safe` tier) means `!` is unavailable.
    let bash_sandbox = sandbox.clone();
    // The MCP config file is read and validated here: before any server process starts and
    // before a session file exists, so a bad file leaves nothing behind (dogfooding D2). The
    // MCP provider itself, the memory service and the coding tools are built by
    // `radian_host::open_session` (ADR 0023 U5).
    let servers = match &cli.mcp_config {
        Some(path) => load_mcp_config(path)?,
        None => Vec::new(),
    };
    // ADR 0026 D9: `--approval` and `--yes` (its shorthand) are the command line's choice.
    // When they are silent, the config's `[permissions] approval` and the journal's last
    // in-session switch decide; `radian_host::resolve_startup` owns that precedence, the
    // `full`-tier rules and the backfill record.
    let cli_approval =
        match &cli.approval {
            Some(name) => Some(radian_permissions::ApprovalPolicy::parse(name).ok_or_else(
                || format!("--approval {name:?} is not one of on-ask, always, never, auto"),
            )?),
            None => cli.yes.then_some(radian_permissions::ApprovalPolicy::Auto),
        };
    let config_approval = config
        .permissions
        .approval
        .as_deref()
        .and_then(radian_permissions::ApprovalPolicy::parse);
    let journal_path = dir.join(format!("{session_id}.jsonl"));
    let restored_approval = resume
        .is_some()
        .then(|| radian_host::restored_approval(&journal_path))
        .flatten();
    let startup_approval = radian_host::resolve_startup(
        cli_approval,
        config_approval,
        restored_approval,
        tier.preset(),
        resume.is_some(),
    );
    let profile =
        radian_tui::profile::build_profile(tier, &workspace_path, &cli.allow_shell_command)?
            .with_approval(startup_approval.policy);

    // D4: panics after this point get a log in the diagnostics directory (ADR 0013 decision
    // 4), not next to the session: the session is the authoritative record and must not be
    // treated as a cache. Set before the terminal is entered; the hook reads it on a panic.
    let panic_log = radian_host::diagnostics_dir().map(|dir| {
        dir.join(radian_host::panic_log_name(
            &session_id,
            radian_host::now_millis(),
        ))
    });
    radian_tui::terminal::set_panic_log(panic_log, Arc::clone(&secrets));

    // The session facts the renderer reads, and the sink that writes them (ADR 0023 U2/U5).
    let projection = Arc::new(radian_host::SessionProjection::new());
    // The renderer reads the session facts from the host's projection, not a private copy
    // (ADR 0023 U5). Set here because the app is created before the session exists (`--pick`).
    app.set_projection(Arc::clone(&projection));
    let live: Arc<dyn EventSink> =
        Arc::new(ProjectionSink::new(tx.clone(), Arc::clone(&projection)));
    // ADR 0023 U7: approvals go through the session's pending queue, so the UI shows one
    // arrival order and never supersedes a request. An `auto` policy (ADR 0026 D9) answers
    // an `ask` before any approver is consulted, so nothing reaches the queue under it.
    let approvals = radian_host::ApprovalRouting::Queued;
    // The audit sink needs the journal, which `open_session` creates; the binding is a factory
    // for that reason.
    let audit: Arc<dyn Fn(Arc<SessionJournal>) -> Arc<dyn AuditSink> + Send + Sync> = {
        let runtime = runtime.handle().clone();
        Arc::new(move |journal: Arc<SessionJournal>| -> Arc<dyn AuditSink> {
            Arc::new(JournalAudit {
                journal,
                runtime: runtime.clone(),
            })
        })
    };
    let runtime_events = {
        let tx = tx.clone();
        let secrets = Arc::clone(&secrets);
        Arc::new(move |event: &radian_session::RuntimeEvent| {
            let redacted = match event {
                radian_session::RuntimeEvent::TurnFailed { turn_id, message } => {
                    let current = radian_tui::terminal::shared_secrets(&secrets);
                    radian_session::RuntimeEvent::TurnFailed {
                        turn_id: turn_id.clone(),
                        message: radian_tui::terminal::redact(message, &current),
                    }
                }
                other => other.clone(),
            };
            let _ = tx.send(UiEvent::Runtime(redacted));
        })
    };
    let driver_stopped = {
        let tx = tx.clone();
        Arc::new(move || {
            let _ = tx.send(UiEvent::DriverStopped);
        })
    };
    let before_driver: Option<Arc<dyn Fn() + Send + Sync>> = cli
        .panic_in_driver
        .then(|| Arc::new(panic_in_driver) as Arc<dyn Fn() + Send + Sync>);
    let memory_warning = Arc::new(|message: &str| eprintln!("radian: [memory] {message}"));

    // ADR 0031 T3: resolve project trust before the session exists. The TUI is interactive, so
    // a workspace with resources and no decision asks through a startup modal (pushed after
    // the terminal is entered); the persistent write + audit happen then, once the journal
    // exists. `--trust`/`--no-trust` override for this process only.
    let cli_trust = if cli.trust {
        Some(radian_host::trust::Decision::Trust)
    } else if cli.no_trust {
        Some(radian_host::trust::Decision::Untrust)
    } else {
        None
    };
    let (trust_setup, trust_warning) =
        radian_host::trust::load_for(&workspace_path, cli_trust, true);
    if let Some(warning) = &trust_warning {
        eprintln!("[trust] {warning}");
    }
    eprintln!("{}", radian_host::trust::status_line(&trust_setup));

    // ADR 0023 U5: the one place a session is established; `agent-cli` calls the same function
    // with its own bindings. Every startup check that can fail has already run, so this is the
    // first thing that creates durable state.
    let mut session = runtime
        .block_on(radian_host::open_session(
            radian_host::SessionSetup {
                config_slot: std::sync::Arc::clone(&config_slot),
                config_queue: radian_runtime::ConfigQueue::new(),
                budget_for: Some(budget_for),
                protected_branches: config.git.protected_branches.clone(),
                session_id: radian_host::SessionId::new(session_id.clone()),
                session_dir: dir.clone(),
                workspace: workspace_path.clone(),
                // The tools are mounted by default now: the workspace is the current directory
                // (or `$HOME/tmp`), not only an explicit `--workspace`.
                coding_root: Some(workspace_path.clone()),
                model,
                servers,
                system_prompt: cli.system.clone(),
                context_budget: window.budget.map(ContextBudget::new),
                auto_compact: !cli.no_auto_compact && config.compaction.auto.unwrap_or(true),
                sandbox,
                tier: tier.preset(),
                profile,
                memory_root,
                skills_extra_paths: radian_host::config::resolve_extra_paths(
                    &config.skills.extra_paths,
                    config.source.as_deref().and_then(std::path::Path::parent),
                    radian_host::user_home().as_deref(),
                ),
                skills_builtin: config.skills.builtin.unwrap_or(true),
                skills_disabled: config.skills.disabled.clone(),
                trust: trust_setup,
                resume: resume.is_some(),
                config_snapshot: serde_json::json!({
                    "provider": provider.as_str(),
                    "model": cli.model,
                }),
                turn_options: TurnOptions {
                    max_model_calls: Some(cli.max_model_calls),
                    ..TurnOptions::default()
                },
                turn_timeout: None,
                wording: radian_host::TUI_WORDING,
                capabilities: radian_host::SessionCapability::ALL.to_vec(),
            },
            radian_host::HostBindings {
                approvals,
                ask_user: true,
                audit,
                events: live,
                projection: Arc::clone(&projection),
                runtime_events,
                driver_stopped,
                before_driver,
                memory_warning,
            },
        ))
        .map_err(|error| error.to_string())?;
    mark("open-session-done");
    // The providers worth refreshing now: the one this run started with. `/model`'s other
    // targets refresh on their next use (the fetch itself is on the loop's runtime).
    let refresh_targets = vec![key_id.clone()];
    // ADR 0026 D0: refresh the switched-to providers' `/v1/models` in the background. It never
    // blocks startup and its failures are silent (the next `/model` just sees the old cache).
    runtime.spawn(async move {
        for provider_id in refresh_targets {
            let Some(key) = radian_host::auth::resolve_key(&provider_id)
                .ok()
                .flatten()
                .map(|resolved| resolved.key)
            else {
                continue;
            };
            // The retry rule (D0): skip when the cache is fresh, or when a fetch failed less
            // than the retry delay ago (the failure marker is an empty `models` write).
            let age = radian_host::provider_models_cache(&provider_id).and_then(|path| {
                std::fs::metadata(path).ok().and_then(|meta| {
                    meta.modified()
                        .ok()
                        .and_then(|t| std::time::SystemTime::now().duration_since(t).ok())
                })
            });
            let cached = radian_host::auth::resolve_key(&provider_id)
                .ok()
                .flatten()
                .and_then(|resolved| {
                    radian_host::read_provider_models(&provider_id, &resolved.key)
                });
            let cached_models = cached.as_ref().map_or(0, |cache| cache.models.len());
            if !radian_host::provider_models_stale(age, cached_models) {
                continue;
            }
            let base_url = radian_host::descriptor(&provider_id)
                .map(|descriptor| descriptor.default_base_url.to_owned())
                .unwrap_or_default();
            match radian_host::fetch_provider_models(&base_url, &key).await {
                Some(models) => {
                    let _ = radian_host::write_provider_models(&provider_id, &key, models);
                }
                None => {
                    // The failure marker: an empty list, so the retry delay applies (D0).
                    let _ = radian_host::write_provider_models(&provider_id, &key, Vec::new());
                }
            }
        }
    });
    // ADR 0026 D8: an explicit `--model`/`--provider` wins over the resumed change, and the
    // override is recorded so the journal stays the authority for the next resume.
    if cli.resume.is_some()
        && let Some((recorded_provider, recorded_model)) = session.model_route()
    {
        let model_differs = cli.model.as_deref().is_some_and(|m| m != recorded_model);
        let provider_differs = cli
            .provider
            .as_deref()
            .is_some_and(|p| p != recorded_provider);
        if model_differs || provider_differs {
            eprintln!(
                "radian: resume recorded {recorded_provider}/{recorded_model}; using the command \
                 line's {provider_name}/{model_name}"
            );
            session
                .queue_config(radian_host::ModelRoute::new(&provider_name, &model_name).to_value());
        }
    }
    // ADR 0026 D9: an explicit command-line choice is recorded, so a resume restores it. The
    // driver journals the change at the next turn boundary, revision after the journal's.
    // The hooks already carry the policy (the profile above), so this is the record only —
    // no audit line: nothing switched inside the session.
    if let Some(policy) = startup_approval.backfill {
        session
            .config_queue()
            .push(serde_json::json!({ "approval": policy.as_str() }));
    }
    // The status bar mirrors the live policy from the first frame; the switch below is the
    // only other writer.
    app.set_auto(startup_approval.policy == radian_permissions::ApprovalPolicy::Auto);
    // The footer's startup facts (ADR 0027 C2): the working directory, the model's display
    // name (the id until a catalog name is known), the provider and the applied thinking
    // level. The git branch is not read here: the `git` contributor refreshes it in the
    // background (ADR 0027 C3), and the draw path never runs git.
    app.set_footer_facts(
        Some(workspace_path.clone()),
        None,
        Some(provider_name.clone()),
        Some(thinking_for_footer),
    );
    // ADR 0027: the hostname, the model's catalog prices (`cost`) and whether
    // automatic compaction is on (`context`'s `{auto}`). Each is a startup fact.
    app.set_hostname(radian_host::hostname());
    app.set_model_price(window.price);
    app.set_bash_available(bash_sandbox.is_some());
    app.set_auto_compact(!cli.no_auto_compact && config.compaction.auto.unwrap_or(true));
    // ADR 0027 C3: the contributors. Their changes wake the UI through the same dirty channel
    // (ADR 0023 U2), and a refresh failure becomes a notice; the UI never waits for one.
    let contributions = radian_host::ContributionStore::new();
    let contrib_tx = tx.clone();
    let on_tick: Arc<dyn Fn(radian_host::Tick) + Send + Sync> = Arc::new(move |tick| {
        let _ = contrib_tx.send(UiEvent::Dirty);
        if let Some(notice) = tick.notice {
            let _ = contrib_tx.send(UiEvent::Command(AppInput::SessionDone(vec![notice])));
        }
    });
    radian_host::spawn_contributor(
        runtime.handle(),
        Arc::new(radian_host::GitContributor::new(workspace_path.clone())),
        Arc::clone(&contributions),
        Arc::clone(&on_tick),
    );
    // ADR 0027 C4 + revision 5: the opencode-go usage contributor. It is spawned only when a
    // key is stored, but it refreshes (and its row shows) only while the **current** model's
    // provider is opencode-go. The watch channel is that gate: the UI flips it on a `/model`
    // switch. The key never leaves this spawn: only the display text enters the store.
    // `usage_period_secs = 0` turns it off; the period is clamped to 15 s–10 min.
    let usage_gate = if let Some(period) = usage_period(config.tui.usage_period_secs)
        && let Ok(Some(key)) = radian_host::auth::resolve_key("opencode-go")
    {
        // The startup model decides the first state (a resumed route wins over the CLI's).
        let startup_provider = session
            .model_route()
            .map(|(provider, _)| provider)
            .unwrap_or_else(|| provider_name.clone());
        let (gate_tx, gate_rx) = tokio::sync::watch::channel(startup_provider == "opencode-go");
        radian_host::spawn_contributor_gated(
            runtime.handle(),
            Arc::new(radian_host::OpencodeGoUsage::new(key.key.clone(), period)),
            Arc::clone(&contributions),
            Arc::clone(&on_tick),
            gate_rx,
        );
        Some(gate_tx)
    } else {
        None
    };
    app.set_contributions(Arc::clone(&contributions));
    // A plain run enters the terminal only now: every fallible check above, and the session
    // itself, have already run. (`--pick` entered it before the picker and keeps it here.)
    let (mut guard, mut terminal) = match startup_terminal {
        Some(pair) => pair,
        None => {
            let pair = enter_terminal(mouse)?;
            spawn_input_thread(tx.clone());
            pair
        }
    };
    mark("terminal-ready");
    // ADR 0023 U7: the queue's changes are a wake-up source. A push, an answer, an abort and
    // a waiter that gave up all bump it, so the UI re-reads the head and the modal matches the
    // queue even when nothing else happened.
    {
        let mut changes = session.queue().subscribe();
        let pending_tx = tx.clone();
        runtime.spawn(async move {
            while changes.changed().await.is_ok() {
                if pending_tx.send(UiEvent::Pending).is_err() {
                    break;
                }
            }
        });
    }
    if cli.panic_after_enter {
        panic!("radian: panic path test");
    }
    if let Some(message) = &cli.panic_message {
        panic!("{message}");
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
        std::fs::write(dir.join("child.pid"), pid.to_string())
            .map_err(|e| format!("child.pid: {e}"))?;
        panic!("radian: panic path test with a child in flight");
    }
    let memory_service = session.memory().cloned();
    // The history of a resumed session is rendered before the first frame, so the transcript
    // shows it from the start. `open_session` already recovered exactly this transcript
    // (`agent_core::recover` over the same journal records `Session::history` folds), so the
    // host takes it instead of re-opening and re-folding the file (resume-perf B).
    let history = std::mem::take(&mut session.history);

    mark("history-read");
    if let Some(path) = &cli.block_input {
        let _ = BLOCK_INPUT.set(path.clone());
    }
    if let Some(path) = &cli.freeze_draw {
        let _ = FREEZE_DRAW.set(path.clone());
    }
    if let Some(millis) = cli.slow_draw {
        let _ = SLOW_DRAW.set(millis);
    }
    if let Some(count) = cli.flood_facts {
        // Test hook (`--flood-facts`): a producer that fills the channel as fast as it can, for
        // the draw-fairness test. Real producers are bounded by the turn structure; this one is
        // not, which is exactly what makes the fairness rule testable.
        let flood = tx.clone();
        std::thread::spawn(move || {
            for index in 0..count {
                let event = AgentEvent::ToolStarted {
                    index: 0,
                    call_id: format!("flood-{index}"),
                    name: "flood".into(),
                };
                if flood.send(UiEvent::Agent(event)).is_err() {
                    return;
                }
            }
        });
    }
    // `/memory` runs against the same service the agent's tools and injection use.
    app.set_memory_enabled(session.facts().memory_enabled);
    for message in &history {
        app.push_message(message);
    }
    mark("entries-built");
    app.prefill_history(&history);
    mark("prefill");
    // A draft from a previous exit is restored once, when resuming the same session.
    if resume.is_some() {
        let path = radian_tui::composer::draft_path(&dir, &session_id);
        match radian_tui::composer::take_draft(&path) {
            Ok(Some(text)) => app.input.set_text(&text),
            Ok(None) => {}
            Err(error) => eprintln!("[draft] cannot read {}: {error}", path.display()),
        }
    }
    // the startup information is one collapsible block (wordmark, one
    // information line, the safety lines that never fold, one Tip) instead of a wall of
    // notices. The full lines are what the notices used to be; the summary keeps each field
    // to a word or two.
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let workspace_line = workspace_root.note.as_ref().map(|note| {
        format!(
            "workspace: {} ({note})",
            radian_tui::workspace::display_coding_root(&workspace_root, home.as_deref())
        )
    });
    // The context window (ADR 0018) and the loaded context files (ADR 0036 C2) are separate
    // lines; `context:` is the files, matching the ADR's wording.
    let context_line = format!("context window: {}", window.describe());
    let session_line = match &resume {
        Some(id) => format!(
            "resumed session {id} ({} messages) · Enter sends · Ctrl-C cancels a turn · \
             Ctrl-C again quits",
            history.len()
        ),
        None => format!(
            "new session {session_id} · Enter sends · Ctrl-C cancels a turn · Ctrl-C again quits"
        ),
    };
    let sandbox_facts = session.facts().sandbox_facts.clone();
    // The host computed this from the real backend (ADR 0027): searching the description text
    // reported `none` on macOS, because the `workspace` tier's sentence does not name it.
    let sandbox_word = session.facts().sandbox_word.clone();
    let budget = window
        .budget
        .map_or_else(|| "unknown".to_string(), |b| b.to_string());
    let mut summary = format!(
        "{} · {sandbox_word} · context {budget} · memory {} · ask_user {}",
        tier.as_str(),
        if session.facts().memory_enabled {
            "on"
        } else {
            "off"
        },
        if session.facts().ask_user {
            "on"
        } else {
            "off"
        },
    );
    if let Some(note) = &workspace_root.note {
        summary.push_str(" · ");
        summary.push_str(note);
    }
    let mut startup_lines = vec![
        format!("profile: {}", session.facts().profile_line),
        sandbox_facts,
        context_line,
        format!("context: {}", session.facts().context_line),
    ];
    if let Some(line) = &workspace_line {
        startup_lines.push(line.clone());
    }
    startup_lines.push(session.facts().memory_notice.clone());
    startup_lines.push(session.facts().ask_user_line());
    // ADR 0026 D9: the startup line names the auto policy whichever way it was chosen.
    if startup_approval.policy == radian_permissions::ApprovalPolicy::Auto {
        startup_lines.push(
            "auto: tool calls are approved without asking; deny rules and the sandbox still \
             apply"
                .to_string(),
        );
    }
    if pick_notice {
        startup_lines.push("no sessions for this workspace yet; starting a new one".to_string());
    }
    startup_lines.push(session_line);
    // ADR 0031 T3: the trust state, in the startup block (same style as `ask_user: off`).
    startup_lines.push(radian_host::trust::status_line(&session.trust()));
    // The safety lines are never folded away: a workspace boundary that
    // was not the user's choice, and every config-file warning/ignored key. The config
    // warnings are also printed before the alternate screen, where a crash can still show
    // them.
    let mut safety = Vec::new();
    if let Some(line) = &workspace_line {
        safety.push(line.clone());
    }
    // The policy warnings (a config `auto` ignored under `full`, a suppressed restore) are
    // never folded either: they explain why the session's approvals behave as they do.
    for warning in &startup_approval.warnings {
        safety.push(warning.clone());
    }
    for warning in &config_warnings {
        safety.push(format!("config: {warning}"));
    }
    let banner = if config.tui.banner.unwrap_or(true) {
        let path = radian_tui::workspace::display_coding_root(&workspace_root, home.as_deref());
        radian_tui::startup::banner_rows(env!("CARGO_PKG_VERSION"), &model_name, &path)
    } else {
        Vec::new()
    };
    let tip = config
        .tui
        .tips
        .unwrap_or(true)
        .then(|| radian_tui::startup::tip_for(&session_id).to_string());
    mark("before-startup");
    app.set_startup(radian_tui::startup::StartupScreen {
        banner,
        banner_narrow: format!(
            "{} radian {}",
            radian_tui::footer::brand_mark(app.footer_glyphs()),
            env!("CARGO_PKG_VERSION")
        ),
        summary,
        lines: startup_lines,
        safety,
        tip,
        expanded: false,
    });
    // ADR 0031 T3: ask once, after the terminal is up and the startup screen is built, when
    // the workspace has project resources and no saved decision. Until it is answered the
    // session is untrusted (nothing project-provided is loaded).
    if session.trust().needs_ask {
        app.push_trust_modal(session.trust().workspace.clone());
    }

    // ADR 0008 revision: `/theme` writes to the file this process read, or to the path it
    // would read when there is no file yet (the write creates it).
    let theme_write_path = config
        .source
        .clone()
        .or_else(|| radian_host::resolved_config_path(cli.config.as_deref()));
    let session_host = SessionHost {
        dir: dir.clone(),
        workspace: workspace_path.to_string_lossy().into_owned(),
        session_id: session_id.clone(),
        secrets: Arc::clone(&secrets),
        journal: Arc::clone(session.journal()),
        config_snapshot: serde_json::json!({
            "provider": provider.as_str(),
            "model": cli.model,
        }),
        theme_colors: config.tui.colors.clone(),
        theme_write_path: theme_write_path.clone(),
        config_read_only: config.read_only,
    };
    mark("startup-set");
    let session = Arc::new(session);
    let facade: Arc<dyn radian_host::SessionFacade> =
        Arc::clone(&session) as Arc<dyn radian_host::SessionFacade>;
    mark("before-ui-loop");
    let outcome = ui_loop(
        &mut terminal,
        &mut app,
        &rx,
        &tx,
        Phase::Session {
            workspace: &workspace_path,
            runtime,
            session: &facade,
            host: &session_host,
            memory: &memory_service,
        },
        (provider_name.clone(), model_name.clone()),
        &config.providers,
        config.tui.theme.clone(),
        cli.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        bash_sandbox,
        usage_gate,
    );
    exit_trace("loop-done");
    // Tell a pending signal task that the loop left on its own, so it does not force-exit.
    radian_tui::signals::confirm_exit();
    guard.restore();
    exit_trace("restored");
    // The draft is written on every clean exit (normal quit, Ctrl-C, SIGTERM/SIGHUP). A panic
    // never reaches here, so the panic path does no file I/O.
    let draft = radian_tui::composer::draft_path(&dir, &session_id);
    if let Err(error) = radian_tui::composer::save_draft(&draft, &app.input.text()) {
        eprintln!("[draft] cannot write {}: {error}", draft.display());
    }

    session.stop();
    // The driver task is the session: if it died, the failure is the app's failure. A panic
    // inside it was caught by the runtime, so `join` reports it, and the message the hook
    // recorded is printed now that the alternate screen is gone.
    let joined = runtime.block_on(session.join());
    exit_trace("stopped");
    match joined {
        radian_host::DriverEnd::Ok => {}
        radian_host::DriverEnd::Failed(error) => return Err(format!("driver: {error}")),
        radian_host::DriverEnd::Panicked(message) => {
            let mut message = format!("the driver task ended unexpectedly: {message}");
            if let Some(panic) = radian_tui::terminal::last_panic() {
                // One line: the normal screen gets one reason line (dogfooding D4).
                message.push_str(": ");
                message.push_str(&panic);
            }
            return Err(message);
        }
    }
    // A failure in the UI loop is the app's failure too; the terminal is already restored.
    let signal = outcome?;
    let saved = format!(
        "session {session_id} saved; {}",
        resume_command(&program_name(), &session_id, &cli, &workspace_path)
    );
    exit_trace("run-done");
    Ok(Some(match signal {
        Some(signal) => format!("received {}; {saved}", radian_tui::signals::name(signal)),
        None => saved,
    }))
}

/// Whether `base_url` points at this machine (loopback), where a server needs no credential.
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
    if let Some(dir) = &cli.session_dir {
        parts.push("--session-dir".to_owned());
        parts.push(shell_arg(&dir.display().to_string()));
    }
    // Only when the workspace is not the current directory: the default is the cwd, so the
    // command works from wherever the user runs it.
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|cwd| std::fs::canonicalize(cwd).ok());
    if cli.workspace.is_some() || cwd.as_deref() != Some(workspace) {
        parts.push("--workspace".to_owned());
        parts.push(shell_arg(&workspace.display().to_string()));
    }
    if let Some(config) = &cli.config {
        parts.push("--config".to_owned());
        parts.push(shell_arg(&config.display().to_string()));
    }
    format!("resume: {}", parts.join(" "))
}

/// Test hook (`--block-input`): a path whose creation marks "the read is now blocked forever".
/// Set once in `run`; `read_input` writes it and parks.
/// Test hook (`--panic-in-driver`): the driver task panics, so the exit path can be checked.
fn panic_in_driver() {
    panic!("radian: driver task panic test");
}

static BLOCK_INPUT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// The `--exit-trace` file (test hook). One `OnceLock` so every call site reads the same
/// path; unset in production, where [`exit_trace`] is a no-op.
static EXIT_TRACE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// [`exit_trace`] at most once per site (the input thread's poll loop runs every 50 ms; the
/// trace must show its *state*, not a flood). Test-only, like the trace itself.
fn exit_trace_once(marked: &std::sync::atomic::AtomicBool, point: &str) {
    // Zero cost when the test hook is off (production): no atomic, no write, no output.
    if EXIT_TRACE.get().is_none() {
        return;
    }
    if !marked.swap(true, std::sync::atomic::Ordering::SeqCst) {
        exit_trace(point);
    }
}

/// Appends one step of the terminal-gone exit path, when the test hook is on. A plain
/// unbuffered append per point, so a hard failure still leaves the earlier points on disk.
fn exit_trace(point: &str) {
    let Some(path) = EXIT_TRACE.get() else {
        return;
    };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        use std::io::Write;
        let _ = writeln!(file, "{point} {nanos}");
    }
}

/// Parks forever when the `--block-input` test hook is on, writing its marker once.
fn block_input_if_asked() {
    let Some(path) = BLOCK_INPUT.get() else {
        return;
    };
    static MARKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !MARKED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        let _ = std::fs::write(path, "blocked\n");
    }
    loop {
        std::thread::park();
    }
}

/// What one input poll found.
enum Input {
    Event(Event),
    Idle,
    /// The terminal is gone: the pty master closed, the ssh session dropped, ...
    Gone,
}

/// The tty's readiness as `poll(2)` sees it.
#[cfg(unix)]
enum Tty {
    Readable,
    Idle,
    /// The master end closed (`POLLHUP`) or the fd is unusable.
    Gone,
}

#[cfg(unix)]
fn tty_readiness(timeout: Duration) -> Tty {
    let mut fd = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: `fd` is a valid `pollfd` for the duration of the call.
    let ready = unsafe { libc::poll(&mut fd, 1, millis) };
    if ready < 0 {
        // EINTR is a signal (SIGWINCH, SIGTERM/SIGHUP): report Idle and let the loop check
        // `signals::received` on its next turn.
        return if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            Tty::Idle
        } else {
            Tty::Gone
        };
    }
    if ready == 0 {
        return Tty::Idle;
    }
    if fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
        return Tty::Gone;
    }
    if fd.revents & libc::POLLIN != 0 {
        Tty::Readable
    } else {
        Tty::Idle
    }
}

/// Asks crossterm for the next event. Safe only when the tty is readable (a pending byte) or
/// crossterm already holds a parsed event: its reader reads the fd, and a read that returns
/// EOF loops forever (see [`read_input`]).
fn crossterm_event() -> Input {
    exit_trace_once(&TRACED_READ, "crossterm-read");
    match event::read() {
        Ok(event) => Input::Event(event),
        Err(_) => Input::Gone,
    }
}

/// Polls stdin for one input event, bounded by `timeout`.
///
/// `crossterm::event::poll`/`read` cannot drive the loop directly: when the terminal has gone
/// away, the Unix event source keeps reading a tty that now returns EOF (`read` = 0) and never
/// returns to the caller, so the app spins forever (crossterm 0.29.0; 16 orphaned `radian-tui`
/// processes at 15–25% CPU). So the readiness check is `poll(2)`, which reports `POLLHUP` once
/// the pty master is closed, and the caller leaves through the ordinary clean-exit path.
///
/// crossterm reads up to 1024 bytes per call and buffers the extra events, so after a readable
/// tty is drained the remaining events must still be returned. **The buffer check is not a
/// safe guard**: `event::poll(0)` returns a buffered event if the parser has one
/// (`crossterm-0.29.0/src/event/source/unix/mio.rs:69`), but with an empty buffer it polls the
/// fd and, when the fd is ready, reads it (`mio.rs:95`); a hung-up tty is "ready" (macOS
/// returns `POLLIN|POLLHUP`), the read returns 0 (EOF), and the inner `loop` (`mio.rs:94-114`)
/// **never leaves** — the read spins on EOF forever. The `signals` task is not a backstop for
/// this either: a pty master close sends no SIGHUP (review, 2026-10-14; the closed-master
/// pty test reproduced a hang whose stack was 424/424 samples inside `event::poll` →
/// `try_read` → `read`).
///
/// Two guards keep it out (both needed; each one's mutation is in the pty test):
/// 1. **Narrowing**: the idle branch calls `event::poll(0)` only when a previous
///    [`crossterm_event`] read may have buffered something ([`MAY_HAVE_BUFFERED`]), and
///    re-checks `tty_readiness(0)` immediately before, so a Close raced since the readiness
///    poll above is reported as [`Input::Gone`] instead of entering crossterm.
/// 2. **Fallback**: the main loop calls [`terminal_is_gone`] (a non-consuming `poll(2)`) on a
///    periodic tick, so even a thread stuck inside crossterm ends the process through the
///    ordinary [`UiEvent::TerminalGone`] path.
#[cfg(unix)]
fn read_input(timeout: Duration) -> Input {
    block_input_if_asked();
    match tty_readiness(timeout) {
        Tty::Gone => Input::Gone,
        Tty::Readable => {
            exit_trace_once(&TRACED_READABLE, "poll-readable");
            let input = crossterm_event();
            if matches!(input, Input::Event(_)) {
                MAY_HAVE_BUFFERED.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            input
        }
        Tty::Idle => {
            exit_trace_once(&TRACED_IDLE, "poll-idle");
            // A window resize reaches crossterm only while it is reading stdin, which an idle
            // app never does: its SIGWINCH pipe is drained inside `event::poll`/`read`. So an
            // idle app stayed at the old width until the next key or wheel ("scroll once and
            // it re-lays out"). Ask the kernel directly instead: `TIOCGWINSZ` does not read
            // stdin, so it cannot hit the EOF spin guard 1 exists for.
            if let Some((rows, cols)) = size_changed() {
                return Input::Event(Event::Resize(rows, cols));
            }
            // Guard 1: do not enter crossterm unless a previous read may have buffered
            // events. Its `poll(0)` reads the fd on an empty buffer and loops on a hung-up
            // tty (see the function docs); a plain Idle is safe.
            if !MAY_HAVE_BUFFERED.load(std::sync::atomic::Ordering::SeqCst) {
                return Input::Idle;
            }
            // The master may have closed since the readiness poll above. The zero-timeout
            // poll is non-consuming, so a Gone here is safe to report.
            if matches!(tty_readiness(Duration::ZERO), Tty::Gone) {
                return Input::Gone;
            }
            match event::poll(Duration::ZERO) {
                Ok(true) => {
                    exit_trace_once(&TRACED_CROSSTERM_ZERO, "poll-crossterm-zero");
                    let input = crossterm_event();
                    if !matches!(input, Input::Event(_)) {
                        MAY_HAVE_BUFFERED.store(false, std::sync::atomic::Ordering::SeqCst);
                    }
                    input
                }
                Ok(false) => {
                    MAY_HAVE_BUFFERED.store(false, std::sync::atomic::Ordering::SeqCst);
                    Input::Idle
                }
                Err(_) => Input::Gone,
            }
        }
    }
}

/// Whether a previous [`crossterm_event`] read may have buffered more events (crossterm reads
/// up to 1024 bytes and parses one event). Only then is it safe to ask crossterm's `poll(0)`;
/// see [`read_input`]'s docs.
static MAY_HAVE_BUFFERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The last terminal size this thread reported, as `rows << 16 | cols` (`0` = never). A window
/// resize is detected by comparing it against a fresh `TIOCGWINSZ`, because crossterm only
/// drains its SIGWINCH pipe while it is reading stdin (see [`read_input`]).
static LAST_SIZE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The current terminal size when it differs from the last one reported, else `None`. `TIOCGWINSZ`
/// only; never a stdin read. The first call records the size without reporting a change.
fn size_changed() -> Option<(u16, u16)> {
    let (cols, rows) = terminal::size().ok()?;
    let packed = (u32::from(rows) << 16) | u32::from(cols);
    let previous = LAST_SIZE.swap(packed, std::sync::atomic::Ordering::SeqCst);
    (previous != 0 && previous != packed).then_some((rows, cols))
}

/// One-per-site markers for the input thread's trace (test hook only).
static TRACED_READABLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static TRACED_IDLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static TRACED_CROSSTERM_ZERO: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static TRACED_READ: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(not(unix))]
fn read_input(timeout: Duration) -> Input {
    block_input_if_asked();
    match event::poll(timeout) {
        Ok(true) => match event::read() {
            Ok(event) => Input::Event(event),
            Err(_) => Input::Gone,
        },
        Ok(false) => Input::Idle,
        Err(_) => Input::Gone,
    }
}

/// Whether an I/O error means the terminal went away (`EIO`/`EPIPE`) rather than a real
/// failure.
fn terminal_gone(error: &std::io::Error) -> bool {
    if matches!(
        error.kind(),
        std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::UnexpectedEof
    ) {
        return true;
    }
    #[cfg(unix)]
    {
        if error.raw_os_error() == Some(libc::EIO) {
            return true;
        }
    }
    false
}

/// Whether the tty is already hung up, as `poll(2)` sees it right now. A draw failure carries
/// crossterm's own "failed to write the buffered data" (no errno), so the tty's own state is
/// the reliable test.
fn terminal_is_gone() -> bool {
    #[cfg(unix)]
    {
        matches!(tty_readiness(Duration::ZERO), Tty::Gone)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Test hook (`--freeze-draw`): once the marker exists, `draw_frame` stops drawing. The app
/// still writes ~30 bytes per frame when idle (the cursor position), so a *content*-static
/// frame is not enough to keep a closed pty off the draw path; this is.
static FREEZE_DRAW: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Test hook (`--slow-draw <MILLIS>`): make every `draw_frame` sleep, so the main loop is
/// genuinely stuck and cannot reach `confirm_exit` before the signal watchdog fires.
static SLOW_DRAW: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// Draws one frame. A draw that fails because the terminal is gone is the same event as
/// [`Input::Gone`]: report it so the caller leaves cleanly instead of erroring out. `Ok(false)`
/// means "the terminal is gone".
fn draw_frame(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
) -> Result<bool, String> {
    if let Some(millis) = SLOW_DRAW.get() {
        std::thread::sleep(Duration::from_millis(*millis));
    }
    if FREEZE_DRAW.get().is_some_and(|path| path.exists()) {
        return Ok(true);
    }
    // `draw/end` is the end of the render closure; the gap to `draw/terminal-done` is
    // ratatui's buffer diff plus the write and flush to the terminal (resume-perf-c: the
    // owner's 1.2 s first-frame gap had no mark here).
    radian_session::trace::mark("draw/terminal-start");
    let drawn = terminal.draw(|frame| radian_tui::render::draw(frame, app));
    radian_session::trace::mark("draw/terminal-done");
    match drawn {
        Ok(_) => {
            if let Some(path) = FREEZE_DRAW.get() {
                let _ = std::fs::write(path, "drawn\n");
            }
            Ok(true)
        }
        Err(error) if terminal_gone(&error) || terminal_is_gone() => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

/// Writes the OSC 52 clipboard sequence for `text` to stdout. Fire-and-forget: the terminal
/// never answers, so the caller's notice says "via OSC 52", never that the clipboard holds
/// the text.
fn write_osc52(text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    out.write_all(osc52(text).as_bytes())?;
    out.flush()
}

/// Feeds `text` to macOS's `pbcopy` and reports whether it accepted it. Only reachable when
/// `tui.clipboard = "pbcopy"` on macOS (anywhere else the config warns and falls back to
/// OSC 52 before the loop ever sees the mode). The pipe write happens on a thread so a large
/// selection cannot deadlock against `pbcopy`'s own buffer.
fn run_pbcopy(text: &str) -> Result<(), String> {
    use std::io::Write;
    let mut child = std::process::Command::new("pbcopy")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| error.to_string())?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let text = text.to_owned();
    let writer = std::thread::spawn(move || {
        stdin
            .write_all(text.as_bytes())
            .and_then(|()| stdin.flush())
            .map_err(|error| error.to_string())
    });
    let status = child.wait().map_err(|error| error.to_string())?;
    writer
        .join()
        .map_err(|_| "the pbcopy writer panicked".to_string())??;
    if status.success() {
        Ok(())
    } else {
        Err(match status.code() {
            Some(code) => format!("exited with status {code}"),
            None => "was terminated by a signal".to_string(),
        })
    }
}

fn spawn_input_thread(tx: Sender<UiEvent>) {
    std::thread::spawn(move || {
        exit_trace("input-thread-start");
        // Record the size the app is about to draw at, so a resize that lands before the
        // first idle poll is still detected (the owner's bare-resize bug).
        if let Ok((cols, rows)) = terminal::size() {
            let packed = (u32::from(rows) << 16) | u32::from(cols);
            LAST_SIZE.store(packed, std::sync::atomic::Ordering::SeqCst);
        }
        loop {
            match read_input(INPUT_POLL) {
                Input::Gone => {
                    exit_trace("pollhup");
                    let _ = tx.send(UiEvent::TerminalGone);
                    return;
                }
                Input::Event(event) => {
                    // A resize burst coalesces to the last one; everything else goes through
                    // the one pure event map (which drops clicks, drags and moves).
                    let event = match event {
                        // crossterm's order is `(cols, rows)`; `coalesce_resize` takes rows first.
                        Event::Resize(cols, rows) => coalesce_resize(rows, cols),
                        other => match radian_tui::events::input_event(other) {
                            Some(event) => event,
                            None => continue,
                        },
                    };
                    if tx.send(UiEvent::Input(event)).is_err() {
                        return;
                    }
                }
                Input::Idle => {}
            }
        }
    });
}

/// A burst of resizes (a window drag) collapses to the last one: every size in between would
/// cost a full re-wrap for a frame nobody sees. A non-resize event stops the collapse and is
/// handled on the next turn (it stays in crossterm's buffer).
fn coalesce_resize(rows: u16, cols: u16) -> InputEvent {
    let (mut rows, mut cols) = (rows, cols);
    while let Input::Event(Event::Resize(next_cols, next_rows)) = read_input(Duration::ZERO) {
        rows = next_rows;
        cols = next_cols;
    }
    // A drag may have moved the window again since; the last size is the one that matters, so
    // report it rather than an intermediate one.
    if let Some((next_rows, next_cols)) = size_changed() {
        rows = next_rows;
        cols = next_cols;
    }
    InputEvent::Resize { rows, cols }
}

/// The loop's side of the world (ADR 0023 U2/U3): everything handling an event or running an
/// effect needs. The app stays pure; this is where the outside lives.
/// What a `Dirty` notification means (ADR 0023 U2): clear the flag **first**, then read the
/// projection, and always draw a frame when the read succeeded. The frame reads the projection
/// for every other fact (turn, usage, budget, tier, MCP), so a fact-only update with an empty
/// streaming buffer still changes what is on screen. `false` means another `Dirty` is already
/// in flight, so this one is coalesced away.
fn apply_dirty(app: &mut App) -> bool {
    match app.projection.take_snapshot() {
        Some(snapshot) => {
            app.update(AppInput::Stream(snapshot));
            true
        }
        None => false,
    }
}

struct LoopContext<'a> {
    /// The startup route (ADR 0026 D11): the fallback when the session has no route yet.
    startup_route: (String, String),
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
    /// The sandbox the user's `!`/`!!` commands run in (ADR 0033): the coding tools' own,
    /// cloned. `None` on the `safe` tier (no shell).
    bash_sandbox: Option<radian_sandbox::Sandbox>,
    /// The running `!` command's cancellation token (Esc, ADR 0033).
    bash_cancel: Option<agent_core::CancellationToken>,
    /// The opencode-go usage gate (ADR 0027): the UI flips it when `/model`
    /// switches the current provider. `None` when there is no key or the period is off.
    usage_gate: Option<tokio::sync::watch::Sender<bool>>,
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
            && let Some(gate) = &self.usage_gate
        {
            let _ = gate.send(route.provider == "opencode-go");
        }
        let result = match event {
            UiEvent::Agent(event) => app.update(AppInput::Event(event)),
            UiEvent::Runtime(event) => app.update(AppInput::Runtime(event)),
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
            UiEvent::DriverStopped => {
                app.note = Some("the driver stopped; leaving".into());
                app.entries.push(radian_tui::Entry::new(
                    radian_tui::EntryKind::Notice,
                    "the driver task ended; the session is closed",
                ));
                app.quit = true;
                // One last frame, so the notice is what the user sees before the screen is
                // restored.
                return true;
            }
        };
        // The completion menu's current-value markers read these; keep them in step with the
        // app (the footer's `reasoning` item is the same value).
        if let Some(level) = app.thinking_level.clone() {
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
                                    candidate.description =
                                        format!("● current · {}", candidate.description);
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
                                candidate.description =
                                    format!("● current · {}", candidate.description);
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
                                })
                                .collect(),
                            Err(error) => {
                                vec![radian_tui::command::Candidate {
                                    insert: String::new(),
                                    description: format!("cannot read auth.json: {error}"),
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
                let checked = match radian_host::auth::resolve_key(&provider) {
                    Ok(Some(_)) => {
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
                    Ok(None) => {
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

#[allow(clippy::too_many_arguments)]
fn ui_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    rx: &Receiver<UiEvent>,
    tx: &Sender<UiEvent>,
    phase: Phase<'_>,
    startup_route: (String, String),
    config_providers: &std::collections::BTreeMap<String, radian_host::ProviderDecl>,
    // The configured theme name (`tui.theme`), for `/theme`'s current-value marker.
    current_theme: Option<String>,
    thinking_max_tokens: u32,
    bash_sandbox: Option<radian_sandbox::Sandbox>,
    // The opencode-go usage gate (ADR 0027), flipped on a `/model` switch.
    usage_gate: Option<tokio::sync::watch::Sender<bool>>,
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
            config_providers: config_providers.clone(),
            current_thinking: app.thinking_level.clone().unwrap_or_default(),
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
    // The pre-session phase ends when the picker resolves; otherwise only `quit` leaves.
    while !app.quit && !app.picking_resolved() {
        // Block until there is something to do. No display in this build changes with time (the
        // status bar, the notes, the find box and the budget line are static text), so there is
        // no `recv_timeout` here: an idle app is not redrawn at all (ADR 0023 U2). A clock-like
        // display would take the timeout branch at this line. The one exception is a live
        // selection drag held at the transcript's top or bottom edge: the auto-scroll is
        // time-driven, so the loop takes a bounded timeout **only while the drag is live**,
        // and goes straight back to blocking `recv` when it ends. The armed Ctrl-C quit
        // window is the other time-driven display (ADR 0023 U2): while it is armed the loop
        // wakes at its deadline to drop the hint.
        let quit_remaining = app
            .quit_pending
            .map(|deadline| deadline.saturating_duration_since(std::time::Instant::now()));
        let first = if app.is_selecting() || quit_remaining.is_some() {
            // The nearer of the two deadlines; `AUTO_SCROLL_TICK` is the drag's own tick.
            let timeout = match (app.is_selecting(), quit_remaining) {
                (true, Some(quit)) => quit.min(AUTO_SCROLL_TICK),
                (true, None) => AUTO_SCROLL_TICK,
                (false, Some(quit)) => quit.max(std::time::Duration::ZERO),
                (false, None) => unreachable!("the branch requires selection or a quit window"),
            }
            // Also wake for the hang-up check, so a drag or an armed quit window never
            // postpones it for longer than one tick.
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
                    if app.is_selecting() && app.mouse_autoscroll() && !draw_frame(terminal, app)? {
                        app.quit = true;
                        break;
                    }
                    continue;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => None,
            }
        } else {
            // A bounded wait, not a blocking one: guard 2's hang-up check runs every tick even
            // while the app is idle. A timeout with the tty alive loops without drawing.
            match rx.recv_timeout(TTY_CHECK_TICK) {
                Ok(event) => Some(event),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if terminal_is_gone() {
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
        let mut dirty = false;
        let mut effects: Vec<Effect> = Vec::new();
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
        // Run the effects (session phase only: the picker produces none). Their results are
        // events, so they are handled by the same code; a result can ask for one more effect
        // (`@` expansion → send), so this repeats until nothing is pending.
        if let Some(ctx) = ctx.as_mut() {
            let mut rounds = 0;
            while !effects.is_empty() {
                rounds += 1;
                if rounds > MAX_EFFECT_ROUNDS {
                    return Err("the effect worklist did not settle".into());
                }
                let batch = std::mem::take(&mut effects);
                let produced = batch.len();
                for effect in batch {
                    ctx.execute(effect)?;
                }
                // Each effect produces at most one result event; take that many (plus whatever
                // else was already queued behind them).
                for _ in 0..produced {
                    match rx.try_recv() {
                        Ok(event) => dirty |= ctx.handle(event, app, &mut effects),
                        Err(_) => break,
                    }
                }
            }
        }
        if dirty {
            if !draw_frame(terminal, app)? {
                // The terminal went away between the input poll and the frame: leave cleanly.
                app.quit = true;
                break;
            }
            // The cursor is visible only while the input box is editable.
            radian_tui::terminal::set_cursor_visible(app.input_editable());
        }
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

/// Where session files live: `--session-dir`, then `RADIAN_SESSION_DIR`, then the platform's
/// *data* directory (never a cache or temp path; ADR 0013 decision 4). This is the same
/// convention `agent-cli` uses, so a session written by one is resumable by the other.
fn session_dir(cli: &Cli) -> Result<PathBuf, String> {
    if let Some(dir) = &cli.session_dir {
        return Ok(dir.clone());
    }
    if let Ok(dir) = std::env::var("RADIAN_SESSION_DIR")
        && !dir.is_empty()
    {
        return Ok(PathBuf::from(dir));
    }
    // ADR 0020: the default is `~/.radian/sessions` (or `$RADIAN_HOME/sessions`).
    radian_host::sessions_dir().ok_or_else(|| {
        "no RADIAN_HOME or HOME to place sessions under; pass --session-dir".to_string()
    })
}

/// The workspace a session belongs to: `--workspace` when given, else the current directory,
/// canonicalized and validated by the shared `radian-host` rule. `--list` filters by the same
/// value, so a session is only listed in the workspace it was created in, and a bad
/// `--workspace` fails as a workspace error (dogfooding D1).
fn session_workspace(cli: &Cli) -> Result<String, String> {
    Ok(resolve_workspace(cli)?.to_string_lossy().into_owned())
}

/// The coding root: `--workspace` as given, else the current directory, with the TUI's two
/// exceptions — `$HOME` becomes `$HOME/tmp` and the filesystem root
/// is refused. The CLI (`agent`) does **not** do this: it is the unattended entry point and gets
/// no write access by default.
fn coding_root(cli: &Cli) -> Result<radian_tui::workspace::CodingRoot, String> {
    let cwd = std::env::current_dir()
        .map_err(|error| format!("cannot read the current directory: {error}"))?;
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    radian_tui::workspace::resolve_coding_root(cli.workspace.as_deref(), &cwd, home.as_deref())
}

/// The canonical, validated workspace path (the shared `radian-host` rule).
fn resolve_workspace(cli: &Cli) -> Result<PathBuf, String> {
    let root = coding_root(cli)?;
    radian_host::resolve_workspace(Some(&root.path))
}

/// `--list`: this workspace's sessions, newest activity first. Same columns as `agent-cli`.
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

/// Why the TUI's `--list` stopped. A closed stdout (`radian --list | head`) is a normal
/// stop, not a failure (dogfooding D21), so it is kept apart from a real error.
enum ListError {
    /// A real failure: exit 1 with this message.
    Failed(String),
    /// stdout's reader went away: exit 0, no message.
    BrokenPipe,
}

impl From<String> for ListError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

impl From<radian_host::StdoutError> for ListError {
    fn from(error: radian_host::StdoutError) -> Self {
        match error {
            radian_host::StdoutError::BrokenPipe => Self::BrokenPipe,
            radian_host::StdoutError::Failed(error) => {
                Self::Failed(format!("cannot write to stdout: {error}"))
            }
        }
    }
}

fn list_sessions(cli: &Cli) -> Result<(), ListError> {
    let dir = session_dir(cli)?;
    let workspace = session_workspace(cli)?;
    if !dir.exists() {
        return Ok(()); // no sessions yet
    }
    radian_session::warn_if_wide(&dir);
    let mut index = Index::open(dir.join("index.db"))
        .map_err(|e| format!("cannot open the session index in {}: {e}", dir.display()))?;
    index
        .rebuild_dir(&dir)
        .map_err(|e| format!("cannot rebuild the session index: {e}"))?;
    let sessions = index
        .list(Some(&workspace))
        .map_err(|e| format!("cannot list sessions: {e}"))?;
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
        radian_host::write_line(&format!(
            "{}\t{}\t{}\t{}",
            session.session_id,
            session.created_ts.as_deref().unwrap_or("-"),
            session.last_ts.as_deref().unwrap_or("-"),
            messages
        ))?;
    }
    Ok(())
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
}
