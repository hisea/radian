//! How a run resolves what it talks to: the model, the tier, the provider, the credential
//! and the request settings (ADR 0026, ADR 0038 decision 3). Split from `cli.rs`'s `run()`
//! (ADR 0043 decision 5): extractions, not pure moves. No behaviour change: the precedence
//! (command line, journal, project, config, built-in) and every refusal run unchanged.

use super::*;

/// What the model/provider phase resolves: every name, route input and credential the
/// session-opening phases need. One struct so `run()` threads a name, not fifteen locals.
pub(super) struct ResolvedModel {
    pub(super) model_name: String,
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

/// `run()`'s model/provider phase: resolves the model name, the tier and its source, the
/// provider, the wire protocol, the credential and the request settings. Every check that
/// can fail runs before the session file exists, so a refusal leaves nothing behind.
pub(super) fn resolve_model(
    cli: &Cli,
    config: &radian_host::Config,
    dir: &Path,
    workspace_path: &Path,
    resume: Option<&str>,
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
    // ADR 0026: a built-in descriptor's model takes its wire protocol
    // from the catalog (the local cache, else the bundled snapshot); a model the catalog does
    // not carry uses the descriptor table. An unsupported `npm` refuses the run.
    let catalog = radian_host::protocol_catalog();
    let protocol = provider_ref.resolve_protocol(Some(&catalog), &model_name)?;
    let provider = protocol.adapter();
    let chatgpt_oauth = provider_name == radian_host::CHATGPT_PROVIDER_ID;
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
