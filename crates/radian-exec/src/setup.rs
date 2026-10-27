//! Startup (`setup`, ADR 0008/0015/0018/0026/0038): resolve the config, the model
//! route, the workspace, the tier and the session id before any session file exists.
//!
//! Split from `lib.rs` by pure move (ADR 0043 decision 5): no behaviour change.

use radian_core::ContextBudget;
use std::path::PathBuf;

use radian_host::{
    DEFAULT_MAX_TOKENS, ModelArgs, Provider, SESSION_PLACEHOLDER, Thinking, build_model,
    check_provider_flags, generate_session_id, is_local_base_url, load_mcp_config, parse_headers,
    resolve_workspace, validate_session_id,
};

use super::inspect::session_dir;
use super::profile;
use super::{Cli, SetupError};

pub(crate) struct Setup {
    pub(crate) model: radian_host::SwitchableModel,
    /// The descriptor id the run started with (the switch vocabulary, ADR 0026 D5).
    pub(crate) provider_id: String,
    pub(crate) config_slot: std::sync::Arc<radian_runtime::ConfigSlot>,
    pub(crate) budget_for: radian_runtime::BudgetFor,
    /// Resolves a route's catalog price from the warm cache, for the resume fold (F3).
    pub(crate) price_for: Option<radian_host::cost::RoutePrice>,
    /// The provider actually chosen, for the session's config snapshot.
    pub(crate) provider: Provider,
    /// The user's `[mcp]` section (empty for `--no-config-mcp`), merged with the project
    /// file and `--mcp-config` in `chat` once trust is resolved.
    pub(crate) mcp_user: radian_host::McpLoad,
    /// The `--mcp-config` file (empty when the flag is absent).
    pub(crate) mcp_file: radian_host::McpLoad,
    pub(crate) mcp_config: Option<String>,
    pub(crate) system_prompt: Option<String>,
    pub(crate) max_model_calls: u32,
    pub(crate) turn_timeout_secs: Option<u64>,
    /// The input budget `B` (ADR 0018), or `None` when no window is known (the budget is
    /// then off, not given a guessed default).
    pub(crate) context_budget: Option<ContextBudget>,
    /// The window and where it came from (ADR 0018), for the startup line.
    pub(crate) window: radian_host::Window,
    /// The settings the CLI and the TUI resolve from the config the same way (ADR 0032 slice 5):
    /// the retries, the compaction settings, the skills, the memory injection and the shell
    /// auto-background.
    pub(crate) settings: radian_host::SessionSettings,
    /// The session id this process uses: the file name, and the `{session_id}` header value.
    pub(crate) session_id: String,
    /// The validated, canonical workspace (dogfooding D1). Resolved in `setup`, before any
    /// session file exists, so a bad `--workspace` fails as a workspace error and never
    /// reaches the sandbox probe or leaves a session behind (dogfooding D2).
    pub(crate) workspace: PathBuf,
    /// The key and every custom header value: redacted from all error output, never printed
    /// and never written to the session file.
    pub(crate) secrets: Vec<String>,
    /// The command-line `--header NAME=VALUE` arguments, unexpanded, for the `subagent` child
    /// (ADR 0043 decision 2): they are secrets, so the tool passes them in the child's
    /// environment, never its argv.
    pub(crate) headers: Vec<String>,
    /// The resolved model name (CLI or config), for the session's config snapshot.
    pub(crate) model_name: String,
    /// The resolved permission profile (CLI flag, config, or the default).
    pub(crate) profile: profile::ProfileArg,
    /// The resolved approval policy and its startup record (ADR 0026 D9).
    pub(crate) approval: radian_host::StartupApproval,
    /// The config file actually read, for the `[config]` startup line.
    pub(crate) config_path: Option<PathBuf>,
    /// Known-but-unwired and unknown config keys, named (never their values) at startup.
    pub(crate) config_skipped: Vec<String>,
    /// `[sandbox] allow_unix_sockets`, `~`-expanded and canonicalized (ADR 0016).
    pub(crate) allow_unix_sockets: Vec<PathBuf>,
    /// `[sandbox] allow_env` (ADR 0034): environment variable names
    /// added to the sandbox whitelist. Names only.
    pub(crate) allow_env: Vec<String>,
    /// `[control] enabled` (ADR 0043): whether this process binds its control
    /// socket at `$RADIAN_HOME/run/<pid>.sock`.
    pub(crate) control_enabled: bool,
    /// The endpoint a subagent child is told to use (`--base-url`): the parent's effective base
    /// URL, only for a provider that takes an override. `None` lets the child resolve it from
    /// the config, the auth entry or the provider default, exactly as the parent did.
    pub(crate) base_url: Option<String>,
    /// The `--image` paths, already read and checked (Q7). Each joins the `-p` prompt's user
    /// message in order; a bad path is a usage error before any session file exists.
    pub(crate) images: Vec<radian_coding::LoadedImage>,
}

pub(crate) async fn setup(
    cli: &Cli,
    child_headers: Result<Vec<String>, String>,
) -> Result<Setup, SetupError> {
    // ADR 0008: the config file provides defaults; every CLI flag wins. A refused file (a
    // plaintext secret, an unknown permission value, an older schema_version) fails startup
    // before any session file exists.
    // ADR 0008 §2: a fresh install has no config file; create the commented default at the
    // default location (never an explicit `--config`/`RADIAN_CONFIG` path). A failure warns
    // once and the run continues.
    match radian_host::ensure_default_config(cli.config.as_deref()) {
        Ok(Some(path)) => eprintln!("created a default config at {}", path.display()),
        Ok(None) => {}
        Err(warning) => eprintln!("[config] {warning}; continuing without creating it"),
    }
    let config = radian_host::load_config(cli.config.as_deref())
        .map_err(|error| SetupError(format!("config: {error}")))?;
    if config.read_only {
        eprintln!(
            "[config] schema_version {} is newer than the supported v{}; reading it read-only",
            config.schema_version.unwrap_or(0),
            radian_host::SUPPORTED_SCHEMA_VERSION
        );
    }
    // ADR 0026 D8: on a resume the journal's last model change outranks the config file (the
    // command line still outranks both, which `model_name` already enforces by being tried
    // first).
    let resumed_route = match cli.resume.as_ref() {
        Some(id) => match session_dir(cli) {
            Ok(dir) => radian_host::resumed_model_route(&dir, id),
            Err(_) => None,
        },
        None => None,
    };
    let model_name = match cli.model.as_deref() {
        Some(model) => model.to_owned(),
        None => resumed_route
            .as_ref()
            .map(|route| route.model.clone())
            .or_else(|| config.model.model.clone())
            .ok_or_else(|| {
                // ADR 0008 §2: `radian exec` with no model still fails non-zero, but the
                // message names the config file (created above or existing) and the
                // `--provider <p> --model <m>` form.
                let path = radian_host::resolved_config_path(cli.config.as_deref())
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "<no config location>".to_owned());
                SetupError(format!(
                    "no model: pass --provider <p> --model <m>, or set model.provider/model in {path}"
                ))
            })?
    };
    // `--model` takes a bare model name; the form is `--provider <p> --model
    // <m>`. A value that looks like a known provider's `<provider>/<model>` with no `--provider`
    // gets one hint on stderr (never a refusal, never a change in meaning).
    if let Some(hint) = radian_host::provider_in_model_hint(
        cli.model.as_deref(),
        cli.provider.as_deref(),
        &config.providers,
    ) {
        eprintln!("{hint}");
    }
    // ADR 0026 D9: `--approval`/`--yes` are the command line's choice; the config and the
    // journal's last in-session switch fill in when it is silent. `full` ignores a config
    // `auto` (with a warning) and never restores one. The TUI runs the same resolution in
    // its own startup; `radian_host::resolve_startup` owns the precedence for both.
    let cli_approval = match &cli.approval {
        Some(name) => Some(
            radian_permissions::ApprovalPolicy::parse(name).ok_or_else(|| {
                SetupError(format!(
                    "--approval {name:?} is not one of on-ask, always, never, auto"
                ))
            })?,
        ),
        None => cli.yes.then_some(radian_permissions::ApprovalPolicy::Auto),
    };
    let config_approval = config
        .permissions
        .approval
        .as_deref()
        .and_then(radian_permissions::ApprovalPolicy::parse);
    let system_prompt = match (&cli.system, &cli.system_file) {
        (Some(text), _) => Some(text.clone()),
        (None, Some(path)) => Some(
            std::fs::read_to_string(path)
                .map_err(|e| SetupError(format!("cannot read --system-file {path}: {e}")))?,
        ),
        _ => None,
    };
    // ADR 0026 D0/D10: `--provider`/config `model.provider` names a provider — a built-in
    // descriptor or a named one (`[providers.<name>]`, or the auth.json entry's `env`). The
    // key is looked up under the **name** (D10), so every custom endpoint has its own slot.
    // ADR 0026 D8: the command line outranks the resumed journal, which outranks the config.
    let provider_name = cli
        .provider
        .clone()
        .or_else(|| resumed_route.as_ref().map(|route| route.provider.clone()))
        .or_else(|| config.model.provider.clone())
        .unwrap_or_else(|| radian_host::GENERIC_ID.to_owned());
    let auth_store = radian_host::AuthStore::open().map_err(|e| SetupError(e.to_string()))?;
    let (provider_ref, provider_warning) = radian_host::resolve_provider(
        &provider_name,
        &config.providers,
        auth_store.entry_env(&provider_name).as_ref(),
    )
    .map_err(SetupError)?;
    if let Some(warning) = &provider_warning {
        eprintln!("[provider] {warning}");
    }
    let key_id = provider_ref.key_id().to_owned();
    // ADR 0026: a built-in descriptor's model takes its wire protocol
    // from the catalog (the local cache, else the bundled snapshot); a model the catalog does
    // not carry uses the descriptor table. An unsupported `npm` is a startup refusal.
    let catalog = radian_host::protocol_catalog();
    let protocol = provider_ref
        .resolve_protocol(Some(&catalog), &model_name)
        .map_err(SetupError)?;
    let provider = protocol.adapter();
    let chatgpt_oauth = provider_name == radian_host::CHATGPT_PROVIDER_ID;
    let base_url_override = cli
        .base_url
        .clone()
        .or_else(|| config.model.base_url.clone());
    // ADR 0026 D11: the unified levels, with the legacy Anthropic spellings accepted for one
    // version (a warning goes to stderr when one is used).
    let (thinking, thinking_budget, legacy_thinking) = match cli.thinking.as_deref() {
        Some(raw) => {
            let (level, budget, legacy) =
                radian_host::parse_thinking_level(raw).map_err(SetupError)?;
            (level.to_owned(), budget, legacy)
        }
        None => (radian_host::DEFAULT_THINKING_LEVEL.to_owned(), None, false),
    };
    if legacy_thinking {
        eprintln!(
            "[thinking] the legacy `--thinking {}` spelling is deprecated; use one of {}",
            cli.thinking.as_deref().unwrap_or_default(),
            radian_host::THINKING_LEVELS.join("|")
        );
    }
    // `--max-tokens` is Anthropic-only; under openai-compatible it would be ignored, so this is
    // a usage error before the session file exists (dogfooding D15). `--thinking` left this
    // check in D11: the unified levels ride both wire protocols.
    check_provider_flags(provider, Thinking::Off, cli.max_tokens).map_err(SetupError)?;
    // ADR 0015: an explicit `--context-window 0` is a usage error, before any session file
    // exists, and the TUI refuses it the same way. The config's `[model] context_window = 0` is
    // the config's spelling of "unset" and never reaches this check (the reader maps it to
    // `None`), so the two readings cannot be confused.
    radian_host::check_context_window_flag(cli.context_window).map_err(SetupError)?;
    // The endpoint this run would talk to, before the session id (for the local-endpoint check).
    let probe_base_url = match base_url_override.clone() {
        Some(url) if provider_ref.takes_base_url_override() => url,
        _ => provider_ref
            .endpoint_for(protocol, None)
            .map_err(SetupError)?,
    };
    // ADR 0026 D2: the key comes only from `auth.json`. A missing entry is a refusal unless the
    // endpoint is local (a local server needs no key).
    let credential = radian_host::resolve_key(&key_id).map_err(|e| SetupError(e.to_string()))?;
    // `resolve_key` only ever resolves an `api_key` entry; `chatgpt`'s login is the oauth
    // entry slice ① wrote (ADR 0026) — `credential` stays `None` for it either
    // way (`ChatGptModel` reads `auth.json` itself, per call), so the refusal below must also
    // check `has_login`, which both shapes share.
    if credential.is_none()
        && !is_local_base_url(&probe_base_url)
        && !radian_host::has_login(&key_id)
    {
        let message = match radian_host::resolve_note(&key_id)
            .map_err(|e| SetupError(e.to_string()))?
        {
            Some(note) => note,
            None => format!(
                "no credential for `{key_id}`: run `radian login {key_id}` (or `/login {key_id}` \
                 in the TUI); refusing to continue because {} is not a local address",
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
    // ADR 0038 decision 3 and ADR 0043 decision 2: the tier and where it came from. The
    // command line wins, then the resumed session's journal, then the remembered per-project
    // record, then the config default, then the built-in `full`. The CLI has no `/access`,
    // but it honors what the TUI remembered and recorded. The project's own declaration can
    // only narrow the result; an explicit `--profile` wins outright and the line names the
    // project's ask beside it.
    let resumed_access = cli.resume.as_ref().and_then(|id| {
        session_dir(cli)
            .ok()
            .and_then(|dir| radian_host::resumed_access(&dir, id))
    });
    let remembered_access = match radian_host::access::remembered_for(&workspace) {
        Ok(tier) => tier,
        Err(warning) => {
            eprintln!("[access] {warning}");
            None
        }
    };
    let project = radian_host::project::read(&workspace);
    for warning in &project.warnings {
        eprintln!("[project] {warning}");
    }
    let (mut profile, mut access_source) = if let Some(explicit) = cli.profile {
        (explicit, "flag")
    } else if let Some(preset) = resumed_access {
        (profile::from_preset(preset), "session")
    } else if let Some(preset) = remembered_access {
        (profile::from_preset(preset), "project")
    } else {
        match config.permissions.profile.as_deref() {
            Some(name) => (profile::parse_profile(name)?, "config"),
            None => (profile::default_profile(), "default"),
        }
    };
    if let Some(declared) = project.tier {
        if cli.profile.is_some() {
            if radian_host::narrow_tier(profile.preset(), declared) != profile.preset() {
                eprintln!(
                    "[access] {} (from flag); the project asks for {}",
                    profile.as_str(),
                    declared.as_str()
                );
            }
        } else {
            let narrowed = radian_host::narrow_tier(profile.preset(), declared);
            if narrowed != profile.preset() {
                profile = profile::from_preset(narrowed);
                access_source = "project config";
            }
        }
    }
    // ADR 0038 decision 3: name where the tier came from when it was not the built-in default,
    // so a remembered, resumed or project-narrowed tier is visible at startup.
    if access_source != "default" {
        eprintln!("[access] {} (from {access_source})", profile.as_str());
    }
    // ADR 0026 D9: the journal's last in-session switch restores auto on `workspace`
    // when the command line was silent. The precedence is `resolve_startup`'s, shared with
    // the TUI.
    let journal_path = session_dir(cli)?.join(format!("{session_id}.jsonl"));
    let restored_approval = cli
        .resume
        .is_some()
        .then(|| radian_host::restored_approval(&journal_path))
        .flatten();
    let startup_approval = radian_host::resolve_startup(
        cli_approval,
        config_approval,
        restored_approval,
        profile.preset(),
        cli.resume.is_some(),
    );
    // The config's headers come first, so a `--header` of the same name wins.
    let mut header_args: Vec<String> = config
        .model
        .headers
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect();
    header_args.extend(cli.header.iter().cloned());
    // ADR 0043 decision 2: a subagent child cannot see the parent's `--header` (argv is
    // world-readable), so the tool passes them through the environment. `run_blocking` already
    // took them out of this process's environment (fix round 1 A), so the values are here and
    // nowhere else; they are still unresolved, so `{session_id}` expands to **this** process's
    // id, exactly as a config file's headers do.
    header_args.extend(child_headers.map_err(SetupError)?);
    let uses_placeholder = header_args
        .iter()
        .any(|item| item.contains(SESSION_PLACEHOLDER));
    let headers = parse_headers(&header_args, &session_id).map_err(SetupError)?;
    if uses_placeholder && cli.session_id.is_none() && cli.resume.is_none() {
        // Only when the generated id is actually observable: it is what makes the routing
        // reusable across processes.
        eprintln!("[session] {session_id} (pass --session-id to reuse it)");
    }
    // ADR 0026 D0/D10: the provider decides the adapter, base URL and headers.
    let route = radian_host::resolve_route_for(
        &provider_ref,
        &model_name,
        base_url_override.as_deref(),
        credential
            .as_ref()
            .and_then(|key| key.env.get(radian_host::RADIAN_BASE_URL))
            .map(String::as_str),
        &headers,
        &session_id,
        Some(&catalog),
    )
    .map_err(SetupError)?;
    // ADR 0026: the catalog overruled the built-in table's protocol.
    if let Some(notice) = &route.protocol_notice {
        eprintln!("[provider] {notice}");
    }
    // The catalog matches on the hostname of the endpoint the model actually talks to (ADR 0018).
    let catalog_base_url = route.base_url.clone();
    let (thinking_of, reasoning_effort) = radian_host::resolve_thinking_ref(
        &provider_ref,
        &model_name,
        &thinking,
        thinking_budget,
        route.adapter(),
        Some(cli.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS)),
    );
    // ADR 0032 slice 5: the settings both hosts read from the config the same way — the retries,
    // the compaction settings (ADR 0018: `[compaction] thinking` converted
    // to this route's wire value, `low` by default; a clamped level is said once), the skills, the
    // memory injection and the shell auto-background.
    let (settings, compaction_notice) = radian_host::SessionSettings::from_config(
        &config,
        &provider_ref,
        &model_name,
        cli.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        cli.no_auto_compact,
    );
    if let Some(notice) = compaction_notice {
        eprintln!("[compaction] {notice}");
    }
    // ADR 0014 R5: the catalog (or a `[providers.<id>]` override) decides whether
    // this route replays reasoning.
    let reasoning_replay = radian_host::resolve_reasoning_replay(&provider_ref, &model_name);
    let replays_reasoning = reasoning_replay.is_some();
    // ADR 0018 §2: the output the request will really send. The chat route sends the
    // catalog-derived `max_tokens` (`min(limit.output, ⌊C/4⌋)`), the Anthropic route its
    // `max_tokens`, and the Responses route none. `R` and `B` count exactly this value.
    let request_output = radian_host::route_output(
        route.adapter(),
        &route.base_url,
        cli.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
    );
    // The catalog groups by provider id: the resolved provider's own id when it is not the
    // generic descriptor (so a `chatgpt` route resolves through the descriptor table, which is
    // the only window source models.dev cannot answer for), else the hostname rule.
    let catalog_provider = cli.catalog_provider.clone().or_else(|| {
        (provider_ref.id() != radian_host::GENERIC_ID).then(|| provider_ref.id().to_owned())
    });
    let window_query = radian_host::WindowQuery::production(
        cli.context_window.or(config.model.context_window),
        catalog_provider.clone(),
        catalog_base_url.clone(),
        model_name.clone(),
        request_output,
        true,
    );
    // The window comes from `--context-window`, the models.dev catalog, or the route's own
    // descriptor table (ADR 0018; a `chatgpt` route has only the last). A fetch failure is not a
    // startup failure: the window is then unknown and the guard stays off. This runs before the
    // session file exists.
    let window = radian_host::resolve(&radian_host::WindowQuery::production(
        cli.context_window.or(config.model.context_window),
        catalog_provider,
        &catalog_base_url,
        &model_name,
        request_output,
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
    if let Some(warning) = radian_host::output_ceiling_warning(window.request_output, window.output)
    {
        eprintln!("{warning}");
    }
    let context_budget = window
        .budget
        .filter(|tokens| *tokens > 0)
        .map(|tokens| ContextBudget::new(tokens).with_reasoning(replays_reasoning));
    let initial = build_model(ModelArgs {
        provider: route.adapter(),
        base_url: route.base_url.clone(),
        model: model_name.clone(),
        api_key: credential.as_ref().map(|key| key.key.clone()),
        headers: route.headers.clone(),
        // ADR 0026 D0: the `opencode-go` descriptor asks for streamed usage by default;
        // `--include-usage` overrides it in either direction.
        include_usage: cli
            .include_usage
            .or_else(|| radian_host::default_include_usage(&provider_name)),
        max_tokens: cli.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        thinking: thinking_of,
        reasoning_effort: reasoning_effort.clone(),
        reasoning_replay: reasoning_replay.clone(),
        max_output_tokens: window.request_output,
        chatgpt_oauth,
    })
    .map_err(SetupError)?;
    // ADR 0026 D5: switchable, like the TUI. The CLI has no `/model`, but the journal can
    // still carry a change the TUI wrote, so resume restores the right model (D8).
    let config_slot = radian_runtime::ConfigSlot::new(serde_json::Value::Null);
    let route_ctx = radian_host::RouteContext {
        providers: config.providers.clone(),
        session_id: session_id.clone(),
        user_headers: headers.clone(),
        include_usage: cli.include_usage,
        max_tokens: cli.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        thinking,
        thinking_budget,
    };
    let model = radian_host::SwitchableModel::new(
        initial,
        radian_host::ModelRoute::new(&provider_name, &model_name),
        config_slot.clone(),
        route_ctx.clone(),
    );
    let budget_for = {
        let query = window_query.clone();
        let providers = config.providers.clone();
        let route_ctx = route_ctx.clone();
        std::sync::Arc::new(move |provider: &str, switch_model: &str| {
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
    // The same lookup gives the **price** (ADR 0027), offline like the budget: the
    // resume fold prices each historical call at the route it ran under (F3).
    let price_for = {
        let query = window_query.clone();
        Some(
            std::sync::Arc::new(move |provider: &str, switch_model: &str| {
                radian_host::resolve_offline(&radian_host::WindowQuery {
                    catalog_provider: Some(provider.to_owned()),
                    model: switch_model.to_owned(),
                    ..query.clone()
                })
                .price
            }) as radian_host::cost::RoutePrice,
        )
    };
    let mcp_file = match &cli.mcp_config {
        Some(path) => load_mcp_config(path).map_err(SetupError)?,
        None => radian_host::McpLoad::default(),
    };
    // ADR 0008 §8: the user's `[mcp]` section and `--mcp-config` load here; they merge with
    // the project `.mcp.json` in `chat`, after trust is resolved (the project file needs
    // the workspace and the trust decision). The `explore` subagent child passes
    // `--no-config-mcp` (with no `--mcp-config` either), so its config section stays
    // empty — and `chat` additionally skips the project file for it.
    let mcp_user = if cli.no_config_mcp {
        radian_host::McpLoad::default()
    } else {
        config.mcp.clone()
    };
    // The key and every header value are secrets: no error this program prints may contain
    // any of them, and they are never written to the session file. "The key" is **every**
    // login in `auth.json`, not only this route's (ADR 0013 T4): a provider
    // error body can echo a key that the adapter does not know to mask, and a session can
    // mention another login. `resolved_keys` is the same list the TUI and the exports use.
    // A value registered after this line (a `chatgpt` token refreshed mid-session) is merged
    // by `redaction_secrets` at each use, so this list is the startup part of it.
    let mut secrets: Vec<String> = radian_host::resolved_keys();
    secrets.extend(route.headers.iter().map(|(_, value)| value.clone()));
    secrets.retain(|secret| !secret.is_empty());
    Ok(Setup {
        model,
        provider_id: provider_name.clone(),
        config_slot,
        budget_for,
        price_for,
        provider,
        mcp_user,
        mcp_file,
        mcp_config: cli.mcp_config.clone(),
        system_prompt,
        max_model_calls: cli.max_model_calls,
        turn_timeout_secs: cli.turn_timeout_secs,
        context_budget,
        window,
        settings,
        session_id,
        workspace,
        secrets,
        headers: cli.header.clone(),
        model_name,
        profile,
        approval: startup_approval,
        config_path: config.source.clone(),
        config_skipped: config.skipped.clone(),
        control_enabled: config.control.enabled,
        base_url: provider_ref
            .takes_base_url_override()
            .then(|| route.base_url.clone()),
        allow_unix_sockets: {
            let (paths, warnings) = radian_host::resolve_allow_unix_sockets(
                &config.sandbox.allow_unix_sockets,
                radian_host::user_home().as_deref(),
            );
            for warning in &warnings {
                eprintln!("[config] {warning}");
            }
            // Slice 0: on Linux the seccomp filter denies every Unix socket, so a
            // non-empty list is ignored (warn once here, at startup).
            if let Some(warning) = radian_host::unix_sockets_ignored_warning(&paths) {
                eprintln!("[config] {warning}");
            }
            paths
        },
        allow_env: config.sandbox.allow_env.clone(),
        images: load_images(cli)?,
    })
}

/// Reads each `--image` path with the host's own bytes and runs the shared image pipeline. A
/// missing file, a non-image, or a limit crossed is a usage error (exit 2, ADR 0032) naming
/// the path and the reason; nothing is read lazily later. The read is bounded
/// (`image::load_path`): a huge non-image never allocates its body, and an over-limit image is
/// refused from metadata before its body is read.
fn load_images(cli: &Cli) -> Result<Vec<radian_coding::LoadedImage>, SetupError> {
    let mut images = Vec::new();
    for path in &cli.image {
        match radian_coding::image::load_path(path) {
            radian_coding::image::PathImage::Loaded(image) => images.push(image),
            radian_coding::image::PathImage::NotImage => {
                return Err(SetupError(format!(
                    "--image {}: not a PNG, JPEG, GIF or WebP image",
                    path.display()
                )));
            }
            radian_coding::image::PathImage::Unreadable(reason)
            | radian_coding::image::PathImage::Unusable(reason) => {
                return Err(SetupError(format!("--image {}: {reason}", path.display())));
            }
        }
    }
    Ok(images)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes
    }

    /// `--image` is repeatable, keeps the command line order, and needs `-p`; a path that is
    /// not an image is refused with the path and the reason (the caller exits 2).
    #[test]
    fn load_images_keeps_order_and_names_a_bad_path() {
        let dir = std::env::temp_dir().join(format!("radian-exec-image-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let first = dir.join("a.png");
        let second = dir.join("b.png");
        let bad = dir.join("notes.png");
        std::fs::write(&first, png(10, 10)).unwrap();
        std::fs::write(&second, png(20, 30)).unwrap();
        std::fs::write(&bad, "plain text").unwrap();

        let cli = Cli::try_parse_from([
            "radian",
            "-p",
            "describe",
            "--image",
            first.to_str().unwrap(),
            "--image",
            second.to_str().unwrap(),
        ])
        .expect("the flags parse");
        let images = load_images(&cli).expect("two valid images");
        assert_eq!(
            images
                .iter()
                .map(|image| image.name.as_str())
                .collect::<Vec<_>>(),
            ["a.png", "b.png"]
        );
        assert_eq!(images[1].height, 30);

        let cli =
            Cli::try_parse_from(["radian", "-p", "describe", "--image", bad.to_str().unwrap()])
                .expect("the flags parse");
        let error = load_images(&cli).unwrap_err();
        assert!(error.0.contains("--image"), "{error:?}");
        assert!(error.0.contains("notes.png"), "{error:?}");
        assert!(error.0.contains("not a PNG"), "{error:?}");

        // Without `-p` the flag is a usage error at parse time.
        assert!(
            Cli::try_parse_from(["radian", "--image", first.to_str().unwrap()]).is_err(),
            "--image needs -p"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A huge sparse non-image is refused from its bounded head: `--image` never allocates the
    /// whole file, and the error is the not-an-image reason (usage exit 2).
    #[test]
    fn a_large_non_image_is_refused_without_reading_it_whole() {
        let dir =
            std::env::temp_dir().join(format!("radian-exec-image-huge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("huge.mp4");
        let mut file = std::fs::File::create(&path).unwrap();
        std::io::Write::write_all(&mut file, b"not an image").unwrap();
        file.set_len(2 * 1024 * 1024 * 1024).unwrap();
        drop(file);

        let cli = Cli::try_parse_from(["radian", "-p", "hi", "--image", path.to_str().unwrap()])
            .expect("the flags parse");
        radian_coding::image::reset_path_read_bytes();
        let error = load_images(&cli).unwrap_err();
        assert!(error.0.contains("not a PNG"), "{error:?}");
        let read = radian_coding::image::path_read_bytes();
        assert!(
            read > 0 && read <= radian_coding::image::MAX_HEAD_BYTES,
            "read {read} bytes of a 2 GiB non-image"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An over-limit image is refused from metadata, before its body is read; the reason is
    /// the documented 5 MiB one (usage exit 2).
    #[test]
    fn an_over_limit_image_is_refused_without_reading_its_body() {
        let dir =
            std::env::temp_dir().join(format!("radian-exec-image-over-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("huge.png");
        let mut file = std::fs::File::create(&path).unwrap();
        std::io::Write::write_all(&mut file, &png(10, 10)).unwrap();
        file.set_len(radian_coding::MAX_IMAGE_BYTES + 1).unwrap();
        drop(file);

        let cli = Cli::try_parse_from(["radian", "-p", "hi", "--image", path.to_str().unwrap()])
            .expect("the flags parse");
        radian_coding::image::reset_path_read_bytes();
        let error = load_images(&cli).unwrap_err();
        assert!(error.0.contains("5 MiB"), "{error:?}");
        let read = radian_coding::image::path_read_bytes();
        assert!(read > 0, "the head must be read");
        assert!(read <= radian_coding::image::MAX_HEAD_BYTES, "read {read}");
        assert!(
            (read as u64) < radian_coding::MAX_IMAGE_BYTES,
            "the body was read"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
