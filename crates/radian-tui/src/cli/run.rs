//! [`run`]: everything between `run_cli` and the event loop - the startup diagnostics, the
//! `--pick` phase, the model/credential/window resolution, the session open (journal, driver,
//! MCP, permissions, trust), the terminal entry, the loop call, and the exit path with its
//! resume line.

use super::herdr::herdr_reporter;
use super::session::{
    JournalAudit, SessionHost, coding_root, list_sessions, prepare_picker, session_dir,
};
use super::*;

pub(super) fn run(
    cli: Cli,
    runtime: &tokio::runtime::Runtime,
    rx: Receiver<UiEvent>,
    tx: Sender<UiEvent>,
) -> Result<Option<String>, RunError> {
    mark("run-start");
    if cli.list {
        return match list_sessions(&cli) {
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
    // ADR 0038 decision 3 and ADR 0043 decision 2: the tier and where it came from. The
    // command line wins, then the resumed session's journal, then the remembered per-project
    // record, then the config default, then the built-in `full`. The remembered record is a
    // per-project, explicitly-confirmed user choice, so it outranks the machine-wide config
    // default. The project's own declaration can only **narrow** the result (ADR 0043); an
    // explicit `--profile` wins outright, and the startup line names the project's ask beside
    // it.
    let resumed_tier = resume
        .as_deref()
        .and_then(|id| radian_host::resumed_access(&dir, id))
        .map(radian_tui::profile::tier_of);
    let remembered_tier = match radian_host::access::remembered_for(&workspace_path) {
        Ok(tier) => tier.map(radian_tui::profile::tier_of),
        Err(warning) => {
            startup_log.line(format!("[access] {warning}"));
            None
        }
    };
    let project = radian_host::project::read(&workspace_path);
    for warning in &project.warnings {
        startup_log.line(format!("[project] {warning}"));
    }
    let cli_tier = cli
        .profile
        .as_deref()
        .map(radian_tui::profile::parse_tier)
        .transpose()?;
    let config_tier = config
        .permissions
        .profile
        .as_deref()
        .map(radian_tui::profile::parse_tier)
        .transpose()?;
    let (mut tier, mut access_source) = if let Some(tier) = cli_tier {
        (tier, radian_tui::profile::AccessSource::Flag)
    } else if let Some(tier) = resumed_tier {
        (tier, radian_tui::profile::AccessSource::Session)
    } else if let Some(tier) = remembered_tier {
        (tier, radian_tui::profile::AccessSource::Project)
    } else if let Some(tier) = config_tier {
        (tier, radian_tui::profile::AccessSource::Config)
    } else {
        (
            radian_tui::profile::Tier::FullAccess,
            radian_tui::profile::AccessSource::Default,
        )
    };
    if let Some(declared) = project.tier {
        if cli_tier.is_some() {
            if radian_host::narrow_tier(tier.preset(), declared) != tier.preset() {
                startup_log.line(format!(
                    "[access] {} (from flag); the project asks for {}",
                    tier.as_str(),
                    declared.as_str()
                ));
            }
        } else {
            let narrowed = radian_host::narrow_tier(tier.preset(), declared);
            if narrowed != tier.preset() {
                tier = radian_tui::profile::tier_of(narrowed);
                access_source = radian_tui::profile::AccessSource::ProjectConfig;
            }
        }
    }
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
        startup_log.line(format!("[provider] {warning}"));
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
        startup_log.line(format!(
            "radian: [thinking] the legacy `--thinking {}` spelling is deprecated; use one of {}",
            cli.thinking.as_deref().unwrap_or_default(),
            radian_host::THINKING_LEVELS.join("|")
        ));
    }
    // `--max-tokens` is Anthropic-only; under openai-compatible it would be ignored, so this is
    // an error before the session file exists (dogfooding D15). `--thinking` left this check in
    // D11: the unified levels ride both wire protocols.
    check_provider_flags(provider, Thinking::Off, cli.max_tokens)?;
    // ADR 0015: the same usage error `radian exec` gives, so the two hosts read `--context-window
    // 0` alike; the config's `[model] context_window = 0` is "unset" in both (the reader already
    // mapped it to `None`).
    check_context_window_flag(cli.context_window)?;
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
        // `resolve_key` only ever resolves an `api_key` entry; `chatgpt`'s login is the oauth
        // entry slice ① wrote (ADR 0026) — `credential` stays `None` for it either
        // way (`ChatGptModel` reads `auth.json` itself, per call).
        None if radian_host::has_login(&key_id) => None,
        None => {
            return Err(RunError::Startup(
                match radian_host::resolve_note(&key_id).map_err(|e| e.to_string())? {
                    Some(note) => note,
                    None => format!(
                        "no credential for `{key_id}`: run `radian login {key_id}`, or \
                         `/login {key_id}` in the TUI; refusing to continue because {} is not a \
                         local address",
                        radian_host::redact_base_url(&probe_base_url)
                    ),
                },
            ));
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
    app.session.model_name = model_name.clone();
    let request_max_output = match provider {
        Provider::Anthropic => Some(cli.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS)),
        // Neither sends an explicit max-output field by default (the Responses adapter only
        // sends `max_output_tokens` when `extra_body` asks for it, same as openai-compatible).
        Provider::OpenAiCompatible | Provider::ChatGpt => None,
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
        radian_host::WindowCheck::Refuse(message) => return Err(RunError::Startup(message)),
        radian_host::WindowCheck::Warn(message) => startup_log.line(message),
    }
    if let Some(warning) = radian_host::output_ceiling_warning(request_max_output, window.output) {
        startup_log.line(warning);
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
    app.session.session_id = session_id.clone();
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
    // ADR 0032 slice 5: the settings the TUI and `radian exec` read from the config the same way
    // — the retries, the compaction settings (ADR 0018: `[compaction]
    // thinking` converted to this route's wire value, `low` by default; a clamped level is said
    // once), the skills, the memory injection and the shell auto-background. The same value
    // builds the task builder below.
    let (settings, compaction_notice) = radian_host::SessionSettings::from_config(
        &config,
        &provider_ref,
        &model_name,
        cli.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        cli.no_auto_compact,
    );
    if let Some(notice) = compaction_notice {
        startup_log.line(format!("radian: [compaction] {notice}"));
    }
    // ADR 0014 Q2: the catalog (or a `[providers.<id>]` override) decides whether
    // this route replays reasoning.
    let reasoning_replay = radian_host::resolve_reasoning_replay(&provider_ref, &model_name);
    let replays_reasoning = reasoning_replay.is_some();
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
        reasoning_effort: reasoning_effort.clone(),
        reasoning_replay: reasoning_replay.clone(),
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
            thinking: thinking.clone(),
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
    let budget_for = {
        let query = window_query.clone();
        let providers = config.providers.clone();
        std::sync::Arc::new(move |provider: &str, switch_model: &str| {
            // The switched route carries its own provider id, so the resolver uses it directly
            // instead of deriving one from the startup base URL.
            let window = radian_host::resolve_offline(&radian_host::WindowQuery {
                catalog_provider: Some(provider.to_owned()),
                model: switch_model.to_owned(),
                ..query.clone()
            });
            // ADR 0014 Q4: the estimate counts reasoning exactly when the route
            // replays it, so the projection matches the request actually sent.
            window.budget.map(|tokens| {
                ContextBudget::new(tokens).with_reasoning(radian_host::route_replays_reasoning(
                    provider,
                    switch_model,
                    &providers,
                ))
            })
        }) as radian_runtime::BudgetFor
    };
    // ADR 0027 + ADR 0026 D5/D7: the same warm-cache lookup gives the **price**, so
    // a switch bills later calls at the new model's rate. Offline like `budget_for`: a switch
    // never fetches, and a cache miss means "no price" (the fee stays hidden), not a guess.
    let route_price = {
        let query = window_query.clone();
        std::sync::Arc::new(move |provider: &str, switch_model: &str| {
            radian_host::resolve_offline(&radian_host::WindowQuery {
                catalog_provider: Some(provider.to_owned()),
                model: switch_model.to_owned(),
                ..query.clone()
            })
            .price
        }) as RoutePrice
    };
    // The permission tier and the sandbox. For an enforced tier a probe failure is an error
    // here, never a fallback to an unsandboxed run (ADR 0012 decision 2). Still before
    // the session file exists, so a refusal leaves nothing behind (dogfooding D2).
    // The tier and its source were resolved above (ADR 0038 decision 3).
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
    // ADR 0038 decision 3: the remembered-tier store is denied read like trust/ — it is the
    // user's map of which projects run unsandboxed. Creating it first lets bwrap mask it.
    if let Some(access) = radian_host::access::access_dir() {
        sandbox_deny_read.push(access);
    }
    let sandbox = radian_tui::profile::build_sandbox(
        tier,
        &workspace_path,
        &sandbox_deny_read,
        &allow_unix_sockets,
        &config.sandbox.allow_env,
    )?;
    // ADR 0016: when the sandbox is one radian inherited from outside,
    // `/access` says so like the startup line does. Read here, while the sandbox is still local
    // (it moves into `open_session` below).
    let inherited_sandbox = sandbox
        .as_ref()
        .is_some_and(|sandbox| sandbox.backend_name() == "inherited");
    // ADR 0038 slice 2: `/access` rebuilds the profile and the sandbox for a new tier through
    // this builder. It captures the same inputs the startup build used, so a switch and a
    // startup cannot drift; a failed sandbox probe is the `Err` that refuses the switch.
    let access_builder: radian_host::AccessBuilder = {
        let workspace = workspace_path.clone();
        let allow_shell = cli.allow_shell_command.clone();
        let deny_read = sandbox_deny_read.clone();
        let sockets = allow_unix_sockets.clone();
        let allow_env = config.sandbox.allow_env.clone();
        std::sync::Arc::new(move |preset: radian_permissions::Preset| {
            radian_tui::profile::build_access(
                preset,
                &workspace,
                &allow_shell,
                &deny_read,
                &sockets,
                &allow_env,
            )
        })
    };
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
    // ADR 0030 slice 8: the process control port. Bound before the session so its fan-out can
    // wrap the host's own sinks; the startup line names the path. A bind failure is a warning,
    // never a startup refusal. The bind creates a tokio listener, and this setup runs outside
    // the async `run` (sessions are opened with `runtime.block_on`), so enter the runtime first.
    let control = {
        let _guard = runtime.enter();
        match radian_host::ControlServer::bind_if(
            config.control.enabled,
            radian_host::radian_home().as_deref(),
        ) {
            Ok(server) => server,
            Err(error) => {
                startup_log.line(format!("[control] disabled: {error}"));
                None
            }
        }
    };
    if let Some(server) = &control {
        startup_log.line(format!("[control] {}", server.path().display()));
    }
    // Held for the whole session: the socket is removed on the normal return and on the panic
    // unwind, even while the driver task still holds a reference to the server (ADR 0030 O6).
    let _control_guard = control.as_ref().map(|server| server.guard());
    let live_inner: Arc<dyn EventSink> =
        Arc::new(ProjectionSink::new(tx.clone(), Arc::clone(&projection)));
    let live: Arc<dyn EventSink> = match &control {
        Some(server) => server.events(live_inner),
        None => live_inner,
    };
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
    let runtime_events_inner = {
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
    let runtime_events: Arc<dyn radian_runtime::RuntimeEvents> = match &control {
        Some(server) => server.runtime_events(runtime_events_inner),
        None => runtime_events_inner,
    };
    let driver_stopped = {
        let tx = tx.clone();
        Arc::new(move |end| {
            let _ = tx.send(UiEvent::DriverStopped(end));
        })
    };
    let before_driver: Option<Arc<dyn Fn() + Send + Sync>> = cli
        .panic_in_driver
        .then(|| Arc::new(panic_in_driver) as Arc<dyn Fn() + Send + Sync>);
    // ADR 0021: a memory line the user must see goes into the transcript as a
    // notice instead of stderr, which the fullscreen UI would draw over. That rule covers a
    // warning too: it fires both at startup (a malformed context file, a bad skill) and
    // mid-turn (a failed memory read), and on the `--pick` path the startup ones would land on
    // the alternate screen.
    let memory_warning = {
        let tx = tx.clone();
        Arc::new(move |message: &str| {
            let _ = tx.send(UiEvent::Command(AppInput::SessionDone(vec![format!(
                "radian: [memory] {message}"
            )])));
        })
    };
    // ADR 0021: the memory tools' notice (the project a default-scope write
    // registered) is a line the user must see, so it goes into the transcript as a notice
    // instead of stderr, which the fullscreen UI would draw over.
    let memory_notice_line = {
        let tx = tx.clone();
        Arc::new(move |line: &str| {
            let _ = tx.send(UiEvent::Command(AppInput::SessionDone(vec![format!(
                "[memory] {line}"
            )])));
        })
    };

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
        startup_log.line(format!("[trust] {warning}"));
    }
    // The status line is in the startup block either way (`trust::status_line` is pushed there
    // once the session exists): inside the alternate screen the block is where it shows.
    startup_log.print_before_screen(&radian_host::trust::status_line(&trust_setup));

    // ADR 0023 U5: the one place a session is established; `agent-cli` calls the same function
    // with its own bindings. Every startup check that can fail has already run, so this is the
    // first thing that creates durable state. ADR 0032 slice 5: `LeadSetup::setup` fills what
    // every host-opened session shares; what the TUI decides itself is written here.
    let mut session = runtime
        .block_on(radian_host::open_session(
            radian_host::LeadSetup {
                settings,
                session_id: radian_host::SessionId::new(session_id.clone()),
                session_dir: dir.clone(),
                workspace: workspace_path.clone(),
                // The tools are mounted by default now: the workspace is the current directory
                // (or `$HOME/tmp`), not only an explicit `--workspace`.
                coding_root: Some(workspace_path.clone()),
                model,
                config_slot: std::sync::Arc::clone(&config_slot),
                budget_for: Some(budget_for),
                price_for: Some(Arc::clone(&route_price)),
                servers,
                system_prompt: cli.system.clone(),
                context_budget: window
                    .budget
                    // ADR 0015: a zero budget is unset, never a real zero-token budget.
                    .filter(|tokens| *tokens > 0)
                    .map(|tokens| ContextBudget::new(tokens).with_reasoning(replays_reasoning)),
                sandbox,
                tier: tier.preset(),
                profile,
                access_builder: Some(access_builder),
                parent: None,
                subagent: Some(radian_host::SubagentParts {
                    executable: std::env::current_exe().ok(),
                    provider: provider_name.clone(),
                    model: model_name.clone(),
                    base_url: provider_ref
                        .takes_base_url_override()
                        .then(|| route.base_url.clone()),
                    config: cli.config.clone(),
                    max_tokens: cli.max_tokens,
                    thinking: cli.thinking.clone(),
                    mcp_config: cli.mcp_config.clone(),
                    trust: trust_setup.resolved.decision.as_bool(),
                }),
                tools_allow: None,
                memory_root,
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
            }
            .setup(),
            radian_host::HostBindings {
                approvals,
                shared_queue: None,
                ask_user: true,
                audit,
                events: live,
                projection: Arc::clone(&projection),
                runtime_events,
                driver_stopped,
                before_driver,
                memory_warning,
                memory_notice_line,
            },
        ))
        .map_err(|error| error.to_string())?;
    mark("open-session-done");
    // ADR 0030 O6: start accepting control requests for this session.
    if let Some(server) = &control {
        server.serve(runtime.handle(), radian_host::ControlTarget::of(&session));
    }
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
            startup_log.line(format!(
                "radian: resume recorded {recorded_provider}/{recorded_model}; using the command \
                 line's {provider_name}/{model_name}"
            ));
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
    // ADR 0038 decision 2 (same rule as /model D8): an explicit `--profile` on a resume wins
    // over the journal's recorded access, and the override is recorded so the journal stays
    // the authority for the next resume.
    if cli.resume.is_some() && cli.profile.is_some() && resumed_tier != cli_tier {
        if let Some(recorded) = resumed_tier {
            startup_log.line(format!(
                "radian: resume recorded tier {}; using the command line's {}",
                recorded.as_str(),
                tier.as_str()
            ));
        }
        session
            .config_queue()
            .push(serde_json::json!({ "access": tier.as_str() }));
    }
    // The status bar mirrors the live policy from the first frame; the switch below is the
    // only other writer.
    app.set_auto(startup_approval.policy == radian_permissions::ApprovalPolicy::Auto);
    // ADR 0038 decision 3: `/access` and the footer show the tier and, when it came from the
    // remembered record, the `(project)` marker. The host set the projection's tier when it
    // opened the session; the marker is added here.
    app.set_access_source(access_source);
    app.set_tier(format!("{}{}", tier.as_str(), access_source.suffix()));
    // ADR 0016: `/access` repeats the startup line's own words for an
    // inherited sandbox (and says nothing extra for any other tier).
    if inherited_sandbox {
        app.set_sandbox_note(Some(session.facts().profile_line.clone()));
    }
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
    // The price must be the one for the route actually in force: a resume where the journal
    // recorded a different model uses **that** model's rate, not the command line's (ADR 0026
    // D8). `window.price` is the startup route's (possibly freshly fetched); a differing
    // resumed route is resolved from the warm cache only, like any other switch.
    let startup_price = match session.model_route() {
        Some((route_provider, route_model))
            if (route_provider.as_str(), route_model.as_str())
                != (provider_name.as_str(), model_name.as_str()) =>
        {
            route_price(&route_provider, &route_model)
        }
        _ => window.price.clone(),
    };
    app.set_model_price(startup_price);
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
    // ADR 0027 C4 + revision 5 + revision 14 decision 3: the provider's usage source comes
    // from its descriptor (`usage = Endpoint`). It is spawned only when a key is stored, but it
    // refreshes (and its row shows) only while the **current** model's provider is
    // opencode-go. The watch channel is that gate: the UI flips it on a `/model` switch. The
    // key never leaves this spawn: only the snapshot enters the store.
    // `usage_period_secs = 0` turns it off; the period is clamped to 15 s–10 min.
    let usage_gate = if let Some(period) = usage_period(config.tui.usage_period_secs)
        && let Ok(Some(key)) = radian_host::auth::resolve_key("opencode-go")
        && let Some(endpoint) = radian_host::usage_endpoint("opencode-go")
    {
        // The startup model decides the first state (a resumed route wins over the CLI's).
        let startup_provider = session
            .model_route()
            .map(|(provider, _)| provider)
            .unwrap_or_else(|| provider_name.clone());
        let (gate_tx, gate_rx) = tokio::sync::watch::channel(startup_provider == "opencode-go");
        radian_host::spawn_usage_gated(
            runtime.handle(),
            (endpoint.build)(&radian_host::UsageArgs {
                url: endpoint.url.to_owned(),
                session_id: session_id.clone(),
                secret: Some(key.key.clone()),
            }),
            period,
            Arc::clone(&contributions),
            Arc::clone(&on_tick),
            gate_rx,
        );
        Some(gate_tx)
    } else {
        None
    };
    // The same mechanism for `chatgpt` (ADR 0026), gated on
    // `has_login("chatgpt")` rather than a resolved api_key (there is none — the source
    // refreshes the oauth entry itself, per call, exactly like `ChatGptModel`).
    let chatgpt_usage_gate = if let Some(period) = usage_period(config.tui.usage_period_secs)
        && radian_host::has_login("chatgpt")
        && let Some(endpoint) = radian_host::usage_endpoint(radian_host::CHATGPT_PROVIDER_ID)
    {
        let startup_provider = session
            .model_route()
            .map(|(provider, _)| provider)
            .unwrap_or_else(|| provider_name.clone());
        let (gate_tx, gate_rx) =
            tokio::sync::watch::channel(chatgpt_usage_active(&startup_provider));
        radian_host::spawn_usage_gated(
            runtime.handle(),
            (endpoint.build)(&radian_host::UsageArgs {
                url: endpoint.url.to_owned(),
                session_id: session_id.clone(),
                secret: None,
            }),
            period,
            Arc::clone(&contributions),
            Arc::clone(&on_tick),
            gate_rx,
        );
        Some(gate_tx)
    } else {
        None
    };
    app.set_contributions(Arc::clone(&contributions));
    // ADR 0027: the out-of-process footer `command` items. Each runs on
    // its own background producer; its value enters the contribution store and nowhere else, so
    // the renderer needs no special case. The command's stdin facts come from a snapshot the loop
    // publishes (a safe handoff, never the app itself), published once here before the first run
    // and again before every frame.
    let footer_context = radian_host::FooterContext::new();
    footer_context.set(app.footer_snapshot());
    app.set_footer_context(Arc::clone(&footer_context));
    for spec in command_specs {
        let once = spec.period.is_none();
        let contributor = Arc::new(radian_host::CommandContributor::new(
            spec,
            Arc::clone(&footer_context),
            session_id.clone(),
            Some(workspace_path.to_string_lossy().into_owned()),
        ));
        if once {
            radian_host::spawn_contributor_once(
                runtime.handle(),
                contributor,
                Arc::clone(&contributions),
                Arc::clone(&on_tick),
            );
        } else {
            radian_host::spawn_contributor(
                runtime.handle(),
                contributor,
                Arc::clone(&contributions),
                Arc::clone(&on_tick),
            );
        }
    }
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
    if let Some(command) = &cli.panic_with_background_job {
        // Test hook: a background job is running when the panic happens. The panic path must
        // kill its whole process group (ADR 0037 decision 6). The registry is reached through
        // the host facade; the command runs under a `full` sandbox (this is a test hook).
        let jobs = session
            .jobs_registry()
            .expect("the session has coding tools");
        let workspace = session.workspace().to_path_buf();
        let policy =
            radian_sandbox::SandboxPolicy::new(radian_sandbox::SandboxMode::FullAccess, &workspace)
                .expect("a full-access policy");
        let sandbox = radian_sandbox::Sandbox::from_policy(policy).expect("a full-access sandbox");
        jobs.start(&sandbox, &workspace, command, None, None)
            .expect("start the test job");
        // The command writes its pid (and its child's) before it sleeps; wait for it, bounded.
        let marker = workspace.join("job.pid");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::fs::read_to_string(&marker).is_err() {
            assert!(
                std::time::Instant::now() < deadline,
                "the test job never wrote job.pid"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("radian: panic path test with a background job in flight");
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
            Err(error) => {
                startup_log.line(format!("[draft] cannot read {}: {error}", path.display()))
            }
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
        "{}{} · {sandbox_word} · context {budget} · memory {} · ask_user {}",
        tier.as_str(),
        access_source.suffix(),
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
    // ADR 0034: report to herdr when this process runs inside a herdr pane. The first report
    // is the current state (idle, or blocked while the trust prompt waits) with the session
    // identity and the resume command.
    let mut herdr = herdr_reporter(
        &config,
        &session_id,
        session.journal_path(),
        &resume_extra_args(&cli, &workspace_path),
    );
    let herdr_note = herdr.as_ref().and_then(|(_, note)| note.clone());
    if let Some((reporter, _)) = herdr.as_mut() {
        reporter.maybe_report(&app);
    }
    let mut startup_lines = vec![
        format!("profile: {}", session.facts().profile_line),
        sandbox_facts,
        context_line,
        format!("context: {}", session.facts().context_line),
    ];
    // ADR 0034 H1: the startup line says reporting is on, and names a rejected resume command.
    if herdr.is_some() {
        startup_lines.push(match &herdr_note {
            Some(reason) => format!("herdr: reporting (no resume: {reason})"),
            None => "herdr: reporting".to_owned(),
        });
    }
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
    // The startup diagnostics the `--pick` path could not print: the
    // alternate screen was already up, so they were collected instead (`StartupLog`). They
    // belong to the startup block, next to the trust line that was already carried here. On a
    // plain run the log collected nothing: the same lines were printed before the terminal.
    startup_lines.extend(startup_log.take());
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
        banner_narrow: format!("{} radian {}", app.brand_mark(), env!("CARGO_PKG_VERSION")),
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
        Arc::clone(&route_price),
        &config.providers,
        config.tui.theme.clone(),
        cli.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        bash_sandbox,
        usage_gate,
        chatgpt_usage_gate,
        herdr.map(|(reporter, _)| reporter),
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
        radian_host::DriverEnd::Failed(error) => {
            return Err(RunError::Failed(format!("driver: {error}")));
        }
        radian_host::DriverEnd::Panicked(message) => {
            let mut message = format!("the driver task ended unexpectedly: {message}");
            if let Some(panic) = radian_tui::terminal::last_panic() {
                // One line: the normal screen gets one reason line (dogfooding D4).
                message.push_str(": ");
                message.push_str(&panic);
            }
            return Err(RunError::Failed(message));
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
