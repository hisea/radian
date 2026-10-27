//! How a run resolves what it talks to: the model, the tier, the provider, the credential
//! and the request settings (ADR 0026, ADR 0038 decision 3). Split from `cli.rs`'s `run()`
//! (ADR 0043 decision 5): extractions, not pure moves. No behaviour change: the precedence
//! (command line, journal, project, config, built-in) and every refusal run unchanged.

use super::*;

/// What the model/provider phase resolves: every name, route input and credential the
/// session-opening phases need. One struct so `run()` threads a name, not fifteen locals.
pub(super) struct ResolvedModel {
    pub(super) model_name: String,
    /// False when no model is configured anywhere (no `--model`, no journaled route, no
    /// `model.model`): the session opens anyway and `/model` is the way in (ADR 0008 §2).
    pub(super) has_model: bool,
    pub(super) tier: radian_tui::profile::Tier,
    pub(super) access_source: radian_tui::profile::AccessSource,
    pub(super) provider_name: String,
    pub(super) provider_ref: radian_host::ProviderRef,
    pub(super) key_id: String,
    pub(super) protocol: radian_host::Protocol,
    pub(super) provider: radian_host::Provider,
    pub(super) chatgpt_oauth: bool,
    pub(super) thinking: String,
    pub(super) thinking_budget: Option<u32>,
    pub(super) thinking_for_footer: String,
    pub(super) credential: Option<radian_host::ResolvedKey>,
    pub(super) base_url_override: Option<String>,
    pub(super) header_args: Vec<String>,
    /// The protocol catalog the route keeps resolving against (C5's `resolve_route_for`).
    pub(super) catalog: radian_host::Catalog,
    /// The tier inputs the `--profile`-on-resume override (C8) re-reads.
    pub(super) resumed_tier: Option<radian_tui::profile::Tier>,
    pub(super) cli_tier: Option<radian_tui::profile::Tier>,
}

/// What `/new` carries from the ending session into the fresh one: the values in force
/// after any in-session `/model`, `/thinking`, `/access` or `/auto` switch — not the
/// config defaults. `None` (a plain startup) leaves the CLI/config/journal chain alone.
#[derive(Debug, Clone)]
pub(super) struct SessionCarry {
    /// The model route in force (`next_model` parsed, else the slot/snapshot route).
    pub(super) route: Option<(String, String)>,
    /// The thinking level in force (`next_thinking`, else the applied level).
    pub(super) thinking: Option<String>,
    /// The tier in force, winning over the CLI flag; the project narrowing still applies.
    pub(super) tier: Option<radian_tui::profile::Tier>,
    /// The live approval policy is Auto (carried like an explicit `--yes`).
    pub(super) auto: bool,
    /// The ending session's id, for the notice.
    pub(super) old_session_id: String,
}

/// `run()`'s model/provider phase: resolves the model name, the tier and its source, the
/// provider, the wire protocol, the credential and the request settings. Every check that
/// can fail runs before the session file exists, so a refusal leaves nothing behind.
pub(super) fn resolve_model(
    cli: &Cli,
    config: &radian_host::Config,
    dir: &Path,
    workspace_path: &Path,
    resume: Option<&str>,
    carry: Option<&SessionCarry>,
    startup_log: &mut StartupLog,
) -> Result<ResolvedModel, RunError> {
    // ADR 0026 D8 + review M1: this runs **after** the picker, so `--pick` and a valueless
    // `--resume` have already decided `resume` and the journal's last model change is what
    // resolves the model, provider, credential, route, window and budget — one path, not a
    // config-based guess that the driver's replay then contradicts. The command line still
    // outranks the journal, which outranks the config.
    // `resume` is the **resolved** id (the picker set it for `--pick`/a valueless `--resume`),
    // not `cli.resume`: reading the journal needs the id the session will actually open with.
    let resumed_route = match resume {
        Some(id) => radian_host::resumed_model_route(dir, id),
        None => None,
    };
    // ADR 0008 §2: no model anywhere is not a refusal in the TUI. The session opens with no
    // model selected; submit is blocked until `/model` queues a route, and the first `/model`
    // writes back to the config file. Resuming keeps the journaled route as today.
    let model_opt = carry
        .and_then(|carry| carry.route.as_ref().map(|(_, model)| model.clone()))
        .or_else(|| cli.model.clone())
        .or_else(|| resumed_route.as_ref().map(|route| route.model.clone()))
        .or_else(|| config.model.model.clone());
    let has_model = model_opt.is_some();
    let model_name = model_opt.unwrap_or_default();
    // `-m/--model` takes a bare model name; the form is `--provider <p>
    // --model <m>`. A value that looks like a known provider's `<provider>/<model>` with no
    // `--provider` gets one startup hint (never a refusal, never a change in meaning).
    if let Some(hint) = radian_host::provider_in_model_hint(
        cli.model.as_deref(),
        cli.provider.as_deref(),
        &config.providers,
    ) {
        startup_log.line(hint);
    }
    // ADR 0038 decision 3 and ADR 0043 decision 2: the tier and where it came from. The
    // command line wins, then the resumed session's journal, then the remembered per-project
    // record, then the config default, then the built-in `full`. The remembered record is a
    // per-project, explicitly-confirmed user choice, so it outranks the machine-wide config
    // default. The project's own declaration can only **narrow** the result (ADR 0043); an
    // explicit `--profile` wins outright, and the startup line names the project's ask beside
    // it.
    let resumed_tier = resume
        .and_then(|id| radian_host::resumed_access(dir, id))
        .map(radian_tui::profile::tier_of);
    let remembered_tier = match radian_host::access::remembered_for(workspace_path) {
        Ok(tier) => tier.map(radian_tui::profile::tier_of),
        Err(warning) => {
            startup_log.line(format!("[access] {warning}"));
            None
        }
    };
    let project = radian_host::project::read(workspace_path);
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
    // `/new` carries the tier in force: it wins over the command line (the switch is
    // newer than the flag), but the project narrowing below still applies (ADR 0043 §3 —
    // the sandbox follows the project-saved value, never a raw carried one).
    let carried_tier = carry.and_then(|carry| carry.tier);
    let (mut tier, mut access_source) = if let Some(tier) = carried_tier {
        (tier, radian_tui::profile::AccessSource::Session)
    } else if let Some(tier) = cli_tier {
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
        // An explicit `--profile` wins outright (the note names the project's ask); a
        // carried tier is still narrowed, like every other non-flag source (ADR 0043 §3).
        if carried_tier.is_none() && cli_tier.is_some() {
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
    // ADR 0008 §2: with no model configured, the dummy route below carries the session
    // until `/model` queues a real one. No credential, no protocol lookup that can fail, no
    // network: `complete` is never reached because submit is blocked while `has_model` is
    // false. The tier above still resolves (sandbox, profile) so the session opens normally.
    if !has_model {
        let provider_name = radian_host::GENERIC_ID.to_owned();
        let (provider_ref, _) =
            radian_host::resolve_provider(&provider_name, &config.providers, None)
                .map_err(|e| e.to_string())?;
        let key_id = provider_ref.key_id().to_owned();
        let catalog = radian_host::protocol_catalog();
        let protocol = provider_ref
            .resolve_protocol(Some(&catalog), &model_name)
            .unwrap_or(radian_host::Protocol::OpenAiCompatible);
        let provider = protocol.adapter();
        let thinking = radian_host::DEFAULT_THINKING_LEVEL.to_owned();
        return Ok(ResolvedModel {
            model_name,
            has_model: false,
            tier,
            access_source,
            provider_name,
            provider_ref,
            key_id,
            protocol,
            provider,
            chatgpt_oauth: false,
            thinking_for_footer: thinking.clone(),
            thinking,
            thinking_budget: None,
            credential: None,
            base_url_override: None,
            header_args: Vec::new(),
            catalog,
            resumed_tier,
            cli_tier,
        });
    }
    // ADR 0026 D0/D10: `--provider`/config `model.provider` names a provider — a built-in
    // descriptor or a named one (`[providers.<name>]` in the config, or the auth.json
    // entry's `env`). The generic `openai-compatible` descriptor is the default (the pre-D0
    // behaviour); the model is never defaulted. The key is looked up under the **name**
    // (D10), so every custom endpoint has its own slot.
    // D8: the command line wins. An explicit `--base-url` is the generic provider's dial
    // (D0), so it counts as the user choosing generic; otherwise the journal's route outranks
    // the config (review M1: this runs after the picker, so the id is known). A carried
    // `/new` route is newer than all of them: the in-session switch already resolved
    // everything around it, so it re-resolves here exactly like a fresh startup flag.
    let carried_route = carry.and_then(|carry| carry.route.clone());
    let provider_name = if let Some((provider, _)) = &carried_route {
        provider.clone()
    } else if cli.base_url.is_some() {
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
    // ADR 0026: a built-in descriptor's model takes its wire protocol
    // from the catalog (the local cache, else the bundled snapshot); a model the catalog does
    // not carry uses the descriptor table. An unsupported `npm` refuses the run.
    let catalog = radian_host::protocol_catalog();
    let protocol = provider_ref.resolve_protocol(Some(&catalog), &model_name)?;
    let provider = protocol.adapter();
    let chatgpt_oauth = provider_name == radian_host::CHATGPT_PROVIDER_ID;
    // ADR 0026 D11: the unified levels, with the legacy Anthropic spellings accepted for one
    // version (a warning goes to stderr when one is used).
    // The in-session switch already won over the command line above, so its thinking level
    // does the same here; the legacy spelling warning still names what the user typed.
    let thinking_arg = carry
        .and_then(|carry| carry.thinking.clone())
        .or_else(|| cli.thinking.clone());
    let (thinking, thinking_budget, legacy_thinking) = match thinking_arg.as_deref() {
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
    let probe_base_url = match base_url_override.clone() {
        Some(url) if provider_ref.takes_base_url_override() => url,
        _ => provider_ref.endpoint_for(protocol, None)?,
    };
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
    Ok(ResolvedModel {
        model_name,
        has_model: true,
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
    })
}

/// What the window/route phase resolves: the window, the session id, the headers, the
/// route and the switchable model every later phase opens the session with. One struct
/// so `run()` threads a name, not ten locals.
pub(super) struct ResolvedRoute {
    pub(super) window: radian_host::Window,
    pub(super) session_id: String,
    pub(super) secrets: radian_tui::terminal::SharedSecrets,
    pub(super) route: radian_host::Route,
    pub(super) replays_reasoning: bool,
    pub(super) settings: radian_host::SessionSettings,
    pub(super) config_slot: std::sync::Arc<radian_runtime::ConfigSlot>,
    pub(super) model: radian_host::SwitchableModel,
    pub(super) budget_for: radian_runtime::BudgetFor,
    pub(super) route_price: RoutePrice,
    /// The parsed `RADIAN_EXTRA_BODY` (ADR 0026 D13), kept for the `/model` switch check:
    /// the same value the routes were built with, checked against the target route's adapter.
    pub(super) extra_body: Option<radian_host::ExtraBody>,
}

/// `run()`'s window/route phase: the status-bar model name, the window fetch and its
/// startup check, the session id, the headers, the route, the thinking level, the shared
/// settings, the switchable model and the offline budget/price lookups.
///
/// The `R` reservation, the base URL and the offline query are recomputed inside, never
/// threaded: only their results cross into the session-opening phases.
#[allow(clippy::too_many_arguments)]
pub(super) fn resolve_window_and_route(
    cli: &Cli,
    config: &radian_host::Config,
    runtime: &tokio::runtime::Runtime,
    app: &mut App,
    resume: &Option<String>,
    carry: Option<&SessionCarry>,
    resolved: &ResolvedModel,
    startup_log: &mut StartupLog,
) -> Result<ResolvedRoute, RunError> {
    // `R` comes from the request's own output setting (ADR 0018 §2): the chat route sends
    // the catalog-derived `max_tokens`, the Anthropic route its `max_tokens`, and the
    // Responses route none.
    // The app was built before the model was resolved (review M1): the status bar's model name
    // is set here, once, from the effective route.
    app.session.model_name = resolved.model_name.clone();
    // The window's base URL: the provider's endpoint (for a named provider, its declaration).
    let catalog_base_url = resolved
        .provider_ref
        .endpoint_for(resolved.protocol, None)?;
    let auth_base_url = resolved
        .credential
        .as_ref()
        .and_then(|credential| credential.env.get(radian_host::RADIAN_BASE_URL))
        .map(String::as_str);
    let output_base_url = match resolved.base_url_override.clone() {
        Some(url) if resolved.provider_ref.takes_base_url_override() => url,
        _ => resolved
            .provider_ref
            .endpoint_for(resolved.protocol, auth_base_url)?,
    };
    let request_output = radian_host::route_output(
        resolved.provider,
        &output_base_url,
        cli.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
    );
    // The catalog groups by provider id; the generic descriptor is not one, so its provider
    // is derived from the base URL hostname. A named provider is its own group only when its
    // name is a catalog provider; otherwise the hostname rule applies (the fallback below).
    let catalog_provider = cli
        .catalog_provider
        .clone()
        .or_else(|| match &resolved.provider_ref {
            radian_host::ProviderRef::BuiltIn(descriptor) => {
                (descriptor.id != radian_host::GENERIC_ID).then(|| descriptor.id.to_string())
            }
            radian_host::ProviderRef::Named(named) => Some(named.name.clone()),
        });
    let window = runtime.block_on(radian_host::resolve(&radian_host::WindowQuery::production(
        cli.context_window.or(config.model.context_window),
        catalog_provider,
        &catalog_base_url,
        resolved.model_name.clone(),
        request_output,
        cli.offline_catalog,
    )));
    // ADR 0018: `C ≤ R` refuses an explicit `--context-window` and only warns for a catalog
    // window (the budget is then off).
    match window.startup_check() {
        radian_host::WindowCheck::Ok => {}
        radian_host::WindowCheck::Refuse(message) => return Err(RunError::Startup(message)),
        radian_host::WindowCheck::Warn(message) => startup_log.line(message),
    }
    if let Some(warning) = radian_host::output_ceiling_warning(window.request_output, window.output)
    {
        startup_log.line(warning);
    }

    // `--resume` with no value: the picker in "resume only" mode.
    // `--pick` (and `--resume` without a value): load the picker's rows **before** the terminal
    // is entered. `open_index` can warn about a wide session directory, and that warning must be
    // on the normal screen. The picker owns its data; the phase itself runs after the terminal
    // (ADR 0023 U4).
    // A carried `/new` session always mints a fresh id: reusing the startup
    // `--session-id` would collide with the session it carries from.
    let session_id = match (&resume, &cli.session_id) {
        (Some(id), _) => id.clone(),
        (None, Some(id)) if carry.is_none() => id.clone(),
        _ => radian_host::generate_session_id(),
    };
    validate_session_id(&session_id)?;
    app.session.session_id = session_id.clone();
    let user_headers = parse_headers(&resolved.header_args, &session_id)?;
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
        &resolved.provider_ref,
        &resolved.model_name,
        resolved.base_url_override.as_deref(),
        resolved
            .credential
            .as_ref()
            .and_then(|credential| credential.env.get(radian_host::RADIAN_BASE_URL))
            .map(String::as_str),
        &user_headers,
        &session_id,
        Some(&resolved.catalog),
    )?;
    // ADR 0026: the catalog overruled the built-in table's protocol.
    if let Some(notice) = &route.protocol_notice {
        startup_log.line(format!("radian: [provider] {notice}"));
    }
    // D1/D2: every check that can fail runs **before** the session file is created. The
    // workspace is validated first, so a typo is a workspace error and never reaches the
    // sandbox probe (dogfooding D1). `radian-host` owns the rule so both hosts share it.
    let (thinking_of, reasoning_effort) = radian_host::resolve_thinking_ref(
        &resolved.provider_ref,
        &resolved.model_name,
        &resolved.thinking,
        resolved.thinking_budget,
        route.adapter(),
        Some(cli.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS)),
    );
    // ADR 0032 slice 5: the settings the TUI and `radian exec` read from the config the same way
    // — the retries, the compaction settings (ADR 0018: `[compaction]
    // thinking` converted to this route's wire value, `low` by default; a clamped level is said
    // once), the skills, the memory injection and the shell auto-background. The same value
    // builds the task builder below.
    let (settings, compaction_notice) = radian_host::SessionSettings::from_config(
        config,
        &resolved.provider_ref,
        &resolved.model_name,
        cli.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        cli.no_auto_compact,
    );
    if let Some(notice) = compaction_notice {
        startup_log.line(format!("radian: [compaction] {notice}"));
    }
    // ADR 0026 D13: `RADIAN_EXTRA_BODY` merges into every request body on every route. It is
    // read once here, before any session file exists; an invalid value is a startup refusal.
    let extra_body = radian_host::read_extra_body()?;
    if let Some(extra) = &extra_body
        && let Some(refusal) = extra.refusal_for(route.adapter())
    {
        // Reserved keys are per route: only the startup route's adapter is consulted here.
        return Err(RunError::from(refusal));
    }
    if let Some(extra) = &extra_body {
        if let Some(line) = extra.applied_line() {
            startup_log.line(line);
        }
        if let Some(line) = extra.settings_warning(&settings.compaction_settings) {
            startup_log.line(line);
        }
    }
    let extra_fields = extra_body
        .clone()
        .map(radian_host::ExtraBody::into_fields)
        .unwrap_or_default();
    // ADR 0014 R5: the catalog (or a `[providers.<id>]` override) decides whether
    // this route replays reasoning.
    let reasoning_replay =
        radian_host::resolve_reasoning_replay(&resolved.provider_ref, &resolved.model_name);
    let replays_reasoning = reasoning_replay.is_some();
    let initial = build_model(ModelArgs {
        provider: route.adapter(),
        base_url: route.base_url.clone(),
        model: resolved.model_name.clone(),
        api_key: resolved
            .credential
            .as_ref()
            .map(|credential| credential.key.clone()),
        headers: route.headers.clone(),
        extra_body: extra_fields.clone(),
        // ADR 0026 D0: the `opencode-go` descriptor asks for streamed usage by default. The
        // TUI has no `--include-usage` flag.
        include_usage: radian_host::default_include_usage(&resolved.provider_name),
        max_tokens: cli.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        thinking: thinking_of,
        reasoning_effort: reasoning_effort.clone(),
        reasoning_replay: reasoning_replay.clone(),
        max_output_tokens: window.request_output,
        chatgpt_oauth: resolved.chatgpt_oauth,
    })?;
    // ADR 0026 D5: the model is switchable. The slot is shared with the driver (open_session
    // wires it), so a `/model` change reaches the next request without touching the key.
    let config_slot = radian_runtime::ConfigSlot::new(serde_json::Value::Null);
    let route_ctx = radian_host::RouteContext {
        providers: config.providers.clone(),
        session_id: session_id.clone(),
        user_headers: user_headers.clone(),
        extra_body: extra_fields,
        include_usage: None,
        max_tokens: cli.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        thinking: resolved.thinking.clone(),
        thinking_budget: resolved.thinking_budget,
    };
    let model = radian_host::SwitchableModel::new(
        initial,
        radian_host::ModelRoute::new(&resolved.provider_name, &resolved.model_name),
        std::sync::Arc::clone(&config_slot),
        route_ctx.clone(),
    );
    // ADR 0026 D7: a switch recomputes `B` from the warm local catalog, never the network.
    let window_query = radian_host::WindowQuery::production(
        cli.context_window.or(config.model.context_window),
        (resolved.provider_name != radian_host::GENERIC_ID).then(|| resolved.provider_name.clone()),
        catalog_base_url.clone(),
        resolved.model_name.clone(),
        request_output,
        true,
    );
    let budget_for = {
        let query = window_query.clone();
        let providers = config.providers.clone();
        let route_ctx = route_ctx.clone();
        std::sync::Arc::new(move |provider: &str, switch_model: &str| {
            // The switched route carries its own provider id, so the resolver uses it directly
            // instead of deriving one from the startup base URL. Its output setting is the
            // switched route's own (ADR 0018 §2), not the startup route's.
            let window = radian_host::resolve_offline(&radian_host::WindowQuery {
                catalog_provider: Some(provider.to_owned()),
                request_output: route_ctx.output_for(provider, switch_model),
                model: switch_model.to_owned(),
                ..query.clone()
            });
            // ADR 0014 R6: the estimate counts reasoning exactly when the route
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
    Ok(ResolvedRoute {
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
        extra_body,
    })
}
