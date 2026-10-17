//! How a run starts: the print-and-exit modes, the config load, the picker preparation
//! and the terminal entered early for `--pick` (ADR 0023 U4). Split from `cli.rs`'s `run()`
//! (ADR 0043 decision 5): extractions, not pure moves — the phases become named functions
//! with the locals they need as parameters. No behaviour change: no reordering of effects.

use super::*;

/// Everything `run()`'s opening phase establishes: the session directory, the workspace,
/// the config, the app, the sandbox inputs and the picker state. Later phases take the
/// fields they need; nothing here is decided twice.
pub(super) struct Startup {
    pub(super) dir: PathBuf,
    pub(super) workspace_root: radian_tui::workspace::CodingRoot,
    pub(super) workspace_path: PathBuf,
    pub(super) config: radian_host::Config,
    pub(super) config_warnings: Vec<String>,
    pub(super) app: App,
    pub(super) allow_unix_sockets: Vec<PathBuf>,
    pub(super) command_specs: Vec<radian_host::CommandSpec>,
    pub(super) mouse: bool,
    pub(super) startup_terminal: Option<(
        radian_tui::terminal::TtyGuard,
        Terminal<CrosstermBackend<std::io::Stdout>>,
    )>,
    pub(super) startup_log: StartupLog,
    pub(super) resume: Option<String>,
    pub(super) pick_notice: bool,
    pub(super) startup_picker: Option<radian_tui::picker::Picker>,
}

/// `run()`'s opening phase: `--list`, the session directory, the workspace, the config,
/// the app and the picker preparation. A plain run has not touched the terminal when this
/// returns; `--pick` (and a valueless `--resume`) has entered it for the picker.
///
/// Returns `None` when `--list` handled the run (print-and-exit): the caller returns
/// `Ok(None)`. Every other path returns the state the later phases continue with.
pub(super) fn startup_preamble(
    cli: &Cli,
    tx: &Sender<UiEvent>,
) -> Result<Option<Startup>, RunError> {
    mark("run-start");
    if cli.list {
        return match list_sessions(cli) {
            Ok(()) | Err(radian_host::ListError::BrokenPipe) => Ok(None),
            // `--list` failing is a real error of this run, not a startup refusal (ADR 0032
            // A3's table: print-and-exit modes return 1 for a real error).
            Err(radian_host::ListError::Failed(message)) => Err(RunError::Failed(message)),
            Err(radian_host::ListError::Write(error)) => {
                Err(RunError::Failed(format!("cannot write to stdout: {error}")))
            }
        };
    }
    // ADR 0013 decision 4: diagnostics live in the user cache directory, and cleanup runs at
    // startup. A failure is one line, never a fault.
    radian_host::clean_startup();
    let dir = session_dir(cli)?;
    let workspace_root = coding_root(cli.workspace.as_deref())?;
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
    // v1 has one configuration layer, chosen by the user (`--config` > `RADIAN_CONFIG` >
    // `~/.radian/config/config.toml`); there is no project-level config (ADR 0031).
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
        return Err(RunError::Startup(format!("config: {error}")));
    }
    // ADR 0026 D8: on a resume the journal's last model change outranks the config file (the
    // command line still outranks both). `--resume`'s id is known by here (`dir` too).
    // `--resume` without a value (and `--pick`) are decided here, before the model block
    // below: `resume` is what the journal route is read from (review M1).
    let resume_only = matches!(cli.resume, Some(None));
    let resume = cli.resume.clone().flatten();
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
        return Err(RunError::Startup(
            "no sessions for this workspace to resume; run without --resume to start one"
                .to_string(),
        ));
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
    // ADR 0027: the configured footer `command` items, parsed once and
    // spawned as background producers later (their values travel only through the contribution
    // store). v1 has no project-level config layer (ADR 0031), so every current source is the
    // user's own and every command is accepted; when one is added, a `command` must be refused
    // there and that slice owns the test.
    let (command_specs, command_warnings) = radian_host::command_specs(&config.tui.footer);
    config_warnings.extend(command_warnings);
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
    // Every startup diagnostic from here on goes through this: printed on the normal screen,
    // or collected for the transcript's startup block once the alternate screen is up (the
    // `--pick` path).
    let mut startup_log = StartupLog { deferred: None };
    if startup_picker.is_some() {
        startup_terminal = Some(enter_terminal(mouse)?);
        startup_log.deferred = Some(Vec::new());
        spawn_input_thread(tx.clone());
    }
    Ok(Some(Startup {
        dir,
        workspace_root,
        workspace_path,
        config,
        config_warnings,
        app,
        allow_unix_sockets,
        command_specs,
        mouse,
        startup_terminal,
        startup_log,
        resume,
        pick_notice,
        startup_picker,
    }))
}

/// How the `--pick` (and valueless `--resume`) pre-session phase ends.
pub(super) enum PickDone {
    /// Leave `run()` with this reason (`None` = the ordinary clean exit).
    Leave(Option<String>),
    /// Continue into the session phase with this `resume` id.
    Continue { resume: Option<String> },
}

/// `run()`'s picker phase (ADR 0023 U4 / plan B): runs the `--pick` overlay in the
/// already-entered terminal until the user chooses. A plain run has no picker and this is
/// a no-op returning the `resume` it was given. The two early exits (a signal, a quit)
/// become `PickDone::Leave`, so the caller returns them unchanged.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_picker_phase(
    app: &mut App,
    startup_picker: Option<radian_tui::picker::Picker>,
    startup_terminal: &mut Option<(
        radian_tui::terminal::TtyGuard,
        Terminal<CrosstermBackend<std::io::Stdout>>,
    )>,
    resume: Option<String>,
    cli: &Cli,
    config: &radian_host::Config,
    rx: &Receiver<UiEvent>,
    tx: &Sender<UiEvent>,
) -> Result<PickDone, RunError> {
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
            &mut *app,
            rx,
            tx,
            Phase::Picking,
            // The picking phase has no resolved model yet (review M1): the route is only a
            // fallback for the `/thinking` clamp, which the picker overlay cannot reach. No
            // session exists, so nothing is billed: the price resolver answers "no price".
            (
                cli.provider.clone().unwrap_or_default(),
                cli.model.clone().unwrap_or_default(),
            ),
            Arc::new(|_: &str, _: &str| None),
            &config.providers,
            config.tui.theme.clone(),
            cli.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            // The picker phase has no session and no sandbox yet.
            None,
            None,
            None,
            // The picker phase has no session to report (ADR 0034 H1).
            None,
        )?;
        if let Some(signal) = signal {
            // A signal during the pre-session phase leaves exactly like the session phase —
            // restore the terminal, exit 128 + signal, print the reason — but there is no
            // session to name, so no `--resume` hint. No session file has been created.
            radian_tui::signals::confirm_exit();
            guard.restore();
            return Ok(PickDone::Leave(Some(format!(
                "received {}",
                radian_tui::signals::name(signal)
            ))));
        }
        app.finish_picking();
        let resume = match app.take_picker_outcome() {
            Some(radian_tui::picker::Outcome::Resume(id)) => {
                return Ok(PickDone::Continue { resume: Some(id) });
            }
            Some(radian_tui::picker::Outcome::New) => resume,
            // `Quit`, Ctrl-C, or the terminal going away before any session: nothing was
            // created, so leave through the ordinary clean-exit path.
            _ => {
                radian_tui::signals::confirm_exit();
                guard.restore();
                return Ok(PickDone::Leave(None));
            }
        };
        return Ok(PickDone::Continue { resume });
    }
    Ok(PickDone::Continue { resume })
}
