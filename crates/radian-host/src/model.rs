//! The shared model construction for both hosts: one [`HostModel`] over the two provider
//! adapters, and one [`build_model`] from resolved arguments.
//!
//! `radian_core::Model::complete` returns `impl Future` (RPITIT), so `Model` is **not object
//! safe**: a host cannot hold a `Box<dyn Model>`. An enum that implements `Model` by matching
//! on its variants keeps static dispatch and lets either provider live behind one type.
//!
//! The host still owns its flags, its key lookup and its secrets; this module only turns the
//! resolved values into a model, so the two hosts cannot build providers differently.

use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use radian_runtime::{ConfigQueue, ConfigSlot, ModelRoute};

use agent_provider_anthropic::{Anthropic, AnthropicConfig, ThinkingConfig};
use agent_provider_openai_compatible::{OpenAiCompatible, OpenAiCompatibleConfig};
use agent_provider_openai_responses::{OpenAiResponses, OpenAiResponsesConfig};
use radian_core::{Model, ModelContext, ModelError, ModelRequest, ModelResponse};
use serde_json::{Map, Value};

use crate::key_pool::PoolKey;
use crate::oauth_chatgpt;

/// The `max_tokens` sent to Anthropic when `--max-tokens` is not given. The Messages API
/// requires the field and defines no server-side default, so the host must pick one. It
/// counts **thinking tokens and the answer together** (wire notes §2.3), so a thinking turn
/// needs room for both: 4096 truncated thinking coding answers, and 16384 is within the output
/// ceiling of the current Claude models. Older models are set smaller with `--max-tokens`.
/// Overridable.
pub const DEFAULT_MAX_TOKENS: u32 = 16384;

/// The per-turn model call limit both `radian exec` and the TUI use when `--max-model-calls`
/// is not given. One constant in the crate both hosts depend on, so
/// the two binaries cannot drift. `radian-core`'s `DEFAULT_MAX_MODEL_CALLS` (32) stays the
/// runtime's own fuse for a host that configures no limit. Overridable with `--max-model-calls`.
pub const DEFAULT_MAX_MODEL_CALLS: u32 = 1000;

/// Anthropic's manual extended thinking requires `budget_tokens >= 1024` and
/// `budget_tokens < max_tokens` (wire notes §2.3). Interleaved thinking, which relaxes this,
/// is not enabled by this adapter, so the regular rule is checked **host-side, before the
/// session file is created**. The adapter still does not validate locally.
pub const MIN_THINKING_BUDGET: u32 = 1024;

/// Which provider wire protocol a host talks. The names are the CLI values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    OpenAiCompatible,
    Anthropic,
    /// The `chatgpt` descriptor's protocol (ADR 0026 slice 3): OpenAI's Responses
    /// API, with the OAuth access token and account id resolved fresh per request rather than
    /// a fixed key (see [`ChatGptModel`]).
    ChatGpt,
}

impl Provider {
    /// The name written into the session's config snapshot (never the key or a header).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiCompatible => "openai-compatible",
            Self::Anthropic => "anthropic",
            Self::ChatGpt => "openai-responses",
        }
    }

    /// The public endpoint used when `--base-url` is omitted. For Anthropic this is
    /// everything before `/v1/messages`; for ChatGpt, everything before `/responses`.
    pub fn default_base_url(self) -> &'static str {
        match self {
            Self::OpenAiCompatible => "https://api.openai.com/v1",
            Self::Anthropic => "https://api.anthropic.com",
            Self::ChatGpt => "https://chatgpt.com/backend-api/codex",
        }
    }

    /// Parses the `--provider` value.
    pub fn from_name(name: &str) -> Result<Self, String> {
        match name {
            "openai-compatible" => Ok(Self::OpenAiCompatible),
            "anthropic" => Ok(Self::Anthropic),
            "openai-responses" => Ok(Self::ChatGpt),
            other => Err(format!(
                "--provider {other:?} is not one of openai-compatible, anthropic, \
                 openai-responses"
            )),
        }
    }
}

/// The `--thinking` value for the Anthropic adapter. `off` sends no `thinking` key, so the
/// request is byte-identical to the pre-thinking adapter (ADR 0014 R10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Thinking {
    #[default]
    Off,
    /// `{"type":"adaptive"}`: the model decides whether and how much to think.
    Adaptive,
    /// `{"type":"enabled","budget_tokens":N}`: manual extended thinking.
    Enabled { budget_tokens: u32 },
}

impl Thinking {
    fn to_config(self) -> Option<ThinkingConfig> {
        match self {
            Self::Off => None,
            Self::Adaptive => Some(ThinkingConfig::Adaptive),
            Self::Enabled { budget_tokens } => Some(ThinkingConfig::Enabled { budget_tokens }),
        }
    }
}

impl FromStr for Thinking {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "off" => Ok(Self::Off),
            "adaptive" => Ok(Self::Adaptive),
            other => match other.strip_prefix("enabled:") {
                Some(budget) => budget
                    .parse::<u32>()
                    .map(|budget_tokens| Self::Enabled { budget_tokens })
                    .map_err(|_| {
                        format!("--thinking {other:?} needs a u32 budget, e.g. enabled:8192")
                    }),
                None => Err(format!(
                    "--thinking {other:?} is not one of off, adaptive, enabled:<budget>"
                )),
            },
        }
    }
}

/// Everything a host resolves before building a model: the flags, the key it read from the
/// environment, and the parsed headers (with `{session_id}` already expanded).
pub struct ModelArgs {
    pub provider: Provider,
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    pub headers: Vec<(String, String)>,
    /// Fields merged into every request body through the adapter's `extra_body` slot, under the
    /// request's own settings (ADR 0026 D13). Empty by default.
    pub extra_body: Map<String, Value>,
    /// `openai-compatible` only (`--include-usage`).
    pub include_usage: Option<bool>,
    /// `anthropic` only.
    pub max_tokens: u32,
    /// `anthropic` only: the resolved thinking setting.
    pub thinking: Thinking,
    /// `openai-compatible` only: the thinking level's wire value (ADR 0026 D11), sent as
    /// `reasoning_effort`. `None` sends no key.
    pub reasoning_effort: Option<String>,
    /// `openai-compatible` only: the catalog-derived output cap sent as `max_tokens`
    /// (ADR 0018). `None` sends no field.
    pub max_output_tokens: Option<u32>,
    /// `openai-compatible` only: the assistant-message field the model requires its reasoning
    /// replayed under (ADR 0014 R4). `None` sends no reasoning.
    pub reasoning_replay: Option<String>,
    /// Whether a [`Provider::ChatGpt`] route authenticates by the `chatgpt` descriptor's OAuth
    /// (ADR 0026) or by a plain `Authorization: Bearer` from `auth.json` (ADR 0026
    /// ). Ignored for the other two protocols.
    pub chatgpt_oauth: bool,
}

/// Either provider behind one `Model`. `Model` is not object safe, so this is an enum and not
/// a `Box<dyn Model>`.
pub enum HostModel {
    OpenAi(OpenAiCompatible),
    Anthropic(Anthropic),
    /// A Responses route over OAuth (`ChatGptModel`).
    ChatGpt(ChatGptModel),
    /// A Responses route over a plain API key (ADR 0026): a built-in
    /// descriptor's `@ai-sdk/openai` model with `Authorization: Bearer <key>` and the
    /// descriptor's own headers, none of the ChatGPT ones.
    Responses(OpenAiResponses),
}

impl Model for HostModel {
    async fn complete(
        &self,
        request: ModelRequest,
        cx: ModelContext<'_>,
    ) -> Result<ModelResponse, ModelError> {
        match self {
            Self::OpenAi(model) => model.complete(request, cx).await,
            Self::Anthropic(model) => model.complete(request, cx).await,
            Self::ChatGpt(model) => model.complete(request, cx).await,
            Self::Responses(model) => model.complete(request, cx).await,
        }
    }
}

/// The `chatgpt` descriptor's model (ADR 0026). Unlike the other two
/// variants, this does **not** wrap a single pre-built adapter: the access token can expire
/// mid-session, so every `complete()` call resolves a fresh token (refreshing it first if
/// it is within [`oauth_chatgpt::REFRESH_MARGIN`] of expiring — slice ①'s `ensure_fresh`),
/// decodes the account id from it (slice ①'s `decode_account_id`, also done fresh each call,
/// per ADR 0026: it is never stored), and only then builds the
/// short-lived [`OpenAiResponses`] adapter that actually sends the request.
///
/// Only the per-call headers are rebuilt: the HTTP client underneath is built once (at
/// construction, with [`agent_provider_openai_responses::build_client`]) and cloned into
/// every adapter — a `reqwest::Client` clone shares the connection pool, so consecutive
/// calls reuse the one TCP/TLS connection instead of handshaking again. The alternative
/// (mutating a cached adapter's headers in place) is not available, since the adapter
/// stores them in an already-built, immutable `HeaderMap`.
pub struct ChatGptModel {
    /// Everything before `/responses` (the same convention [`OpenAiResponsesConfig::base_url`]
    /// documents). Overridable so a test points it at a local mock; production always uses
    /// the descriptor's fixed endpoint.
    pub base_url: String,
    pub model: String,
    /// The descriptor's static headers only (`OpenAI-Beta`, `originator`, `session_id` —
    /// `chatgpt_headers` in `provider.rs`), **not** `Authorization`/`chatgpt-account-id`.
    pub static_headers: Vec<(String, String)>,
    /// Fields merged into every request body (ADR 0026 D13), under the request settings.
    pub extra_body: Map<String, Value>,
    pub reasoning_effort: Option<String>,
    /// The HTTP client every call sends through, built once at construction (a cheap `Arc`
    /// clone per call shares its connection pool).
    pub client: reqwest::Client,
    /// Where the OAuth refresh/token calls go; a test points this at a local fake server too
    /// (production always uses [`oauth_chatgpt::Endpoints::default`]).
    pub endpoints: oauth_chatgpt::Endpoints,
}

impl Model for ChatGptModel {
    async fn complete(
        &self,
        request: ModelRequest,
        cx: ModelContext<'_>,
    ) -> Result<ModelResponse, ModelError> {
        let access = oauth_chatgpt::ensure_fresh(&self.endpoints)
            .await
            .map_err(|error| ModelError::Protocol(error.to_string()))?;
        let account_id = oauth_chatgpt::decode_account_id(&access)
            .map_err(|error| ModelError::Protocol(error.to_string()))?;
        let mut config = OpenAiResponsesConfig::new(&self.base_url, &self.model);
        for (name, value) in &self.static_headers {
            config = config.with_header(name.clone(), value.clone());
        }
        config = config
            .with_header("Authorization", format!("Bearer {access}"))
            .with_header("chatgpt-account-id", account_id);
        config.extra_body = self.extra_body.clone();
        config.reasoning_effort = self.reasoning_effort.clone();
        let model = OpenAiResponses::with_client(config, self.client.clone()).map_err(|error| {
            ModelError::Protocol(format!("could not build the ChatGPT request: {error}"))
        })?;
        model.complete(request, cx).await
    }
}

/// The configured endpoint as it may appear in an error: credentials and query values are
/// removed (ADR 0013 T4). The provider crate owns the rule; this is the host's name for it so
/// both hosts redact identically. The two adapters' implementations are identical.
pub fn redact_base_url(raw: &str) -> String {
    agent_provider_openai_compatible::redact_url(raw)
}

/// Checks the Anthropic-only flags against the chosen provider.
///
/// `--thinking` and `--max-tokens` only mean something to the Anthropic adapter. Under
/// `openai-compatible` they would be **silently ignored**, which is worse than a clear usage
/// error, so the host refuses them before any session file is created. The hosts pass the raw
/// flag values: `max_tokens` is `None` when the user did not pass it, so the default is not
/// mistaken for an explicit choice.
pub fn check_provider_flags(
    provider: Provider,
    thinking: Thinking,
    max_tokens: Option<u32>,
) -> Result<(), String> {
    if provider == Provider::Anthropic {
        return Ok(());
    }
    let mut offending = Vec::new();
    if thinking != Thinking::Off {
        offending.push("--thinking");
    }
    if max_tokens.is_some() {
        offending.push("--max-tokens");
    }
    if offending.is_empty() {
        return Ok(());
    }
    let flags = offending.join(" and ");
    Err(format!(
        "{flags} only appl{} to --provider anthropic and would be ignored under {}; remove \
         {flags}, or pass --provider anthropic",
        if offending.len() == 1 { "ies" } else { "y" },
        provider.as_str(),
    ))
}

/// Resolves a thinking **level** against a route's map (ADR 0026 D11): the Anthropic-side
/// `Thinking` and the OpenAI-side `reasoning_effort` it builds into. Shared by the startup
/// build and by every switch, so the two paths cannot drift.
pub fn resolve_thinking(
    descriptor_id: &str,
    model: &str,
    level: &str,
    adapter: Provider,
    max_tokens: Option<u32>,
) -> (Thinking, Option<String>) {
    let map = crate::descriptor(descriptor_id)
        .and_then(|descriptor| crate::thinking_map_for(descriptor, model));
    resolve_thinking_map(map, level, None, adapter, max_tokens)
}

/// The shared resolution: `legacy_budget` is a manual `enabled:<budget>` the user typed, which
/// is sent as-is for Anthropic (ADR 0026 D11) instead of the level's own budget — an
/// over-limit manual budget stays a usage error, never a clamp (review B1).
///
/// A **level's** own budget is clamped to `[MIN_THINKING_BUDGET, max_tokens - 1]` (B1): the
/// number is Radian's choice, not the user's, so a small `--max-tokens` must not fail the
/// turn. `max_tokens <= MIN_THINKING_BUDGET` makes every `enabled` level unfitting, and the
/// clamp walks past it.
fn resolve_thinking_map(
    map: Option<crate::ThinkingMap>,
    level: &str,
    legacy_budget: Option<u32>,
    adapter: Provider,
    max_tokens: Option<u32>,
) -> (Thinking, Option<String>) {
    if let Some(budget_tokens) = legacy_budget
        && adapter == Provider::Anthropic
    {
        return (Thinking::Enabled { budget_tokens }, None);
    }
    match crate::clamp_thinking_with(map, level, max_tokens) {
        // ChatGPT's `reasoning.effort` is the same plain-string shape as `reasoning_effort`
        // (the adapter sends the wire value as-is, ADR 0026 slice 3) — it must not
        // fall into the Anthropic-shaped branch below.
        Some((_, wire)) if matches!(adapter, Provider::OpenAiCompatible | Provider::ChatGpt) => {
            (Thinking::Off, Some(wire.to_owned()))
        }
        Some((_, wire)) => {
            let thinking = thinking_from_wire(wire);
            // Clamp the level's own budget to what the request can hold (B1). `level_fits`
            // already refused a level that cannot fit at all, so this only trims.
            let thinking = match (thinking, max_tokens, crate::wire_budget(wire)) {
                (Thinking::Enabled { budget_tokens }, Some(max), Some(_)) => Thinking::Enabled {
                    budget_tokens: budget_tokens.min(max.saturating_sub(1)),
                },
                (thinking, _, _) => thinking,
            };
            (thinking, None)
        }
        None => (Thinking::Off, None),
    }
}

/// The Anthropic wire value a thinking map carries, as the adapter's `Thinking` (ADR 0026 D11):
/// an empty value sends no `thinking` key, `adaptive` is the existing setting, and
/// `enabled:<budget>` is a manual budget.
fn thinking_from_wire(wire: &str) -> Thinking {
    match wire {
        "" => Thinking::Off,
        "adaptive" => Thinking::Adaptive,
        other => other
            .strip_prefix("enabled:")
            .and_then(|budget| budget.parse::<u32>().ok())
            .map(|budget_tokens| Thinking::Enabled { budget_tokens })
            // A wire value the Anthropic adapter does not know sends **nothing**: inventing a
            // setting for a map that never named it would violate D11's rule.
            .unwrap_or(Thinking::Off),
    }
}

/// [`resolve_thinking`] for a [`crate::ProviderRef`]: a named provider uses the map of the
/// descriptor its kind names (ADR 0026 D10/D11).
pub fn resolve_thinking_ref(
    provider: &crate::ProviderRef,
    model: &str,
    level: &str,
    legacy_budget: Option<u32>,
    adapter: Provider,
    max_tokens: Option<u32>,
) -> (Thinking, Option<String>) {
    resolve_thinking_map(
        crate::thinking_map_for_ref(provider, model),
        level,
        legacy_budget,
        adapter,
        max_tokens,
    )
}

/// The `[compaction] thinking` value that means "do not override the conversation's level"
/// (ADR 0018).
pub const COMPACTION_THINKING_CURRENT: &str = "current";

/// Builds the request-level settings the **compaction** (summary) call carries, from the
/// configured `[compaction] thinking` level (ADR 0018). Returns
/// `(settings, notice)`:
///
/// * `settings` is the object for `radian_core::AgentConfig::compaction_settings` — the wire
///   value the route's protocol expects (`{"reasoning_effort":…}` for OpenAI-compatible,
///   `{"thinking":…}` for Anthropic) — or [`serde_json::Value::Null`] when the call must carry
///   no setting: the level is [`COMPACTION_THINKING_CURRENT`], the route's map supports no
///   level, or the level maps to "send nothing" (`off`).
/// * `notice` is a one-line clamp message when the requested level was moved to the nearest
///   supported one (or the route supports none). A supported level yields `None`.
///
/// The level is clamped with the **same map `/thinking` uses** (`clamp_thinking_with`, ADR
/// 0026 D11), so the summary call's level is clamped exactly like the conversation's.
pub fn compaction_settings_for(
    provider: &crate::ProviderRef,
    model: &str,
    level: &str,
    max_tokens: u32,
) -> (serde_json::Value, Option<String>) {
    if level == COMPACTION_THINKING_CURRENT {
        return (serde_json::Value::Null, None);
    }
    let adapter = provider.protocol_for(model).adapter();
    let map = crate::thinking_map_for_ref(provider, model);
    // The level actually applied (`None` when the route supports no level at all), for the
    // clamp notice.
    let applied = crate::clamp_thinking_with(map, level, Some(max_tokens)).map(|(level, _)| level);
    let (thinking, effort) = resolve_thinking_map(map, level, None, adapter, Some(max_tokens));
    let settings = match adapter {
        // The Responses adapter reads the same `reasoning_effort` request-level key
        // (`agent-provider-openai-responses/src/request.rs`), so this is one branch, not two.
        Provider::OpenAiCompatible | Provider::ChatGpt => {
            match effort.filter(|wire| !wire.is_empty()) {
                Some(wire) => serde_json::json!({ "reasoning_effort": wire }),
                None => serde_json::Value::Null,
            }
        }
        Provider::Anthropic => match thinking {
            Thinking::Off => serde_json::Value::Null,
            Thinking::Adaptive => serde_json::json!({ "thinking": { "type": "adaptive" } }),
            Thinking::Enabled { budget_tokens } => serde_json::json!({
                "thinking": { "type": "enabled", "budget_tokens": budget_tokens }
            }),
        },
    };
    let notice = match applied {
        Some(applied) if applied == level => None,
        Some(applied) => {
            let supported =
                crate::supported_thinking_levels_ref_with(provider, model, Some(max_tokens));
            Some(format!(
                "thinking {level} → {applied} (this model supports {})",
                supported.join("/")
            ))
        }
        // The route supports no level at all: there is nothing to clamp to and nothing to
        // say — the summary call simply carries no setting, exactly like the conversation.
        None => None,
    };
    (settings, notice)
}

/// [`compaction_settings_for`] for a route named by `(provider_name, model)`, resolving the
/// provider from the config's declarations exactly as the per-route budget resolver does
/// (ADR 0026 D7). The driver calls it at the turn boundary so the summary call's settings
/// follow a `/model` switch (ADR 0018): a form frozen at startup (`reasoning_effort`
/// on an Anthropic route, or the reverse) is rejected by the provider and the compaction fails.
/// An unknown provider yields `Null` — no setting is invented.
pub fn compaction_settings_for_route(
    providers: &std::collections::BTreeMap<String, crate::ProviderDecl>,
    provider_name: &str,
    model: &str,
    level: &str,
    max_tokens: u32,
) -> serde_json::Value {
    match resolve_route_name(
        provider_name,
        model,
        providers,
        UnsupportedProtocol::BuiltInTable,
    ) {
        Ok(route) => compaction_settings_for(&route.provider_ref, model, level, max_tokens).0,
        Err(_) => serde_json::Value::Null,
    }
}

/// The reasoning-replay field for a route (ADR 0014 R5): the `[providers.<id>]`
/// override when the provider is named, else the catalog's `interleaved` field — the local
/// cache when it parses, else the bundled snapshot (ADR 0018's order; ADR 0014 R5). `None`
/// means the model does not require replay, or declares a field Radian does not replay (e.g.
/// `reasoning_details`).
pub fn resolve_reasoning_replay(provider: &crate::ProviderRef, model: &str) -> Option<String> {
    if let crate::ProviderRef::Named(named) = provider
        && let Some(override_) = &named.reasoning_replay
    {
        return match override_ {
            crate::provider::ReasoningReplayOverride::Field(field) => Some(field.clone()),
            crate::provider::ReasoningReplayOverride::Off => None,
        };
    }
    crate::catalog::catalog_reasoning_replay(provider.id(), model)
}

/// Whether a route replays reasoning, resolved by provider **name** (ADR 0014 R5):
/// the `[providers.<name>]` override, else the catalog's `interleaved` field (cache, else the
/// bundled snapshot). This is what the per-route budget resolver uses, so the estimate counts
/// the reasoning the request really carries. A name reachable only through `auth.json`'s `env`
/// has no declaration here and no catalog entry, so it does not replay.
pub fn route_replays_reasoning(
    provider_name: &str,
    model: &str,
    providers: &std::collections::BTreeMap<String, crate::ProviderDecl>,
) -> bool {
    match resolve_route_name(
        provider_name,
        model,
        providers,
        UnsupportedProtocol::BuiltInTable,
    ) {
        Ok(route) => resolve_reasoning_replay(&route.provider_ref, model).is_some(),
        Err(_) => crate::catalog::catalog_reasoning_replay(provider_name, model).is_some(),
    }
}

/// A route name resolved by [`resolve_route_name`] (ADR 0026 D10): the provider the name
/// names, the auth entry's `env` it came with, and the model's protocol.
struct ResolvedRouteName {
    provider_ref: crate::ProviderRef,
    auth_env: Option<std::collections::BTreeMap<String, String>>,
    protocol: crate::Protocol,
}

/// What a model whose catalog `npm` Radian cannot speak does (ADR 0026): the switch builder
/// must refuse it, while a budget or reserved-key query has no protocol to use and falls back
/// to the built-in table.
#[derive(Clone, Copy)]
enum UnsupportedProtocol {
    Refuse,
    BuiltInTable,
}

/// [`resolve_route_name`]'s implementation: the auth entry's env and model list, then the
/// provider, then the model's protocol (ADR 0026 D10). The model list is attached so the
/// route's thinking map and endpoint follow the per-model resolution.
fn resolve_route_name(
    provider_name: &str,
    model: &str,
    providers: &std::collections::BTreeMap<String, crate::ProviderDecl>,
    unsupported: UnsupportedProtocol,
) -> Result<ResolvedRouteName, String> {
    let auth_env = crate::AuthStore::open()
        .ok()
        .and_then(|store| store.entry_env(provider_name));
    let model_list = crate::auth::resolve_key(provider_name)
        .ok()
        .flatten()
        .and_then(|resolved| crate::read_provider_models(provider_name, &resolved.key));
    let (provider_ref, _warning) = crate::resolve_provider_with_models(
        provider_name,
        providers,
        auth_env.as_ref(),
        model_list,
    )?;
    let catalog = crate::protocol_catalog();
    let protocol = match provider_ref.resolve_protocol(Some(&catalog), model) {
        Ok(protocol) => protocol,
        Err(error) => match unsupported {
            UnsupportedProtocol::Refuse => return Err(error),
            UnsupportedProtocol::BuiltInTable => provider_ref.protocol_for(model),
        },
    };
    Ok(ResolvedRouteName {
        provider_ref,
        auth_env,
        protocol,
    })
}

/// The adapter a route resolves to, by provider **name** (ADR 0026 D10). `None` when the name
/// resolves to no provider. Used to check `RADIAN_EXTRA_BODY`'s reserved keys against the
/// route a switch would build (ADR 0026 D13).
pub fn route_adapter_for(
    provider_name: &str,
    model: &str,
    providers: &std::collections::BTreeMap<String, crate::ProviderDecl>,
) -> Option<Provider> {
    resolve_route_name(
        provider_name,
        model,
        providers,
        UnsupportedProtocol::BuiltInTable,
    )
    .ok()
    .map(|route| route.protocol.adapter())
}

/// Builds the provider adapter named by `args.provider`. The error is the same shape the
/// hosts already printed for a bad `--base-url`.
pub fn build_model(args: ModelArgs) -> Result<HostModel, String> {
    let base_url = args.base_url.as_str();
    // Any message that names the endpoint must redact it: `--base-url` can carry userinfo or
    // a query token, and an error is as readable as Debug output (ADR 0013 T4).
    let shown_url = redact_base_url(base_url);
    match args.provider {
        Provider::OpenAiCompatible => {
            let mut config = OpenAiCompatibleConfig::new(base_url, &args.model);
            if let Some(key) = &args.api_key {
                config = config.with_api_key(agent_provider_openai_compatible::ApiKey::new(key));
            }
            config.headers = args.headers;
            config.extra_body = args.extra_body;
            config.include_usage = args.include_usage;
            config.max_tokens = args.max_output_tokens;
            config.reasoning_effort = args.reasoning_effort;
            config.reasoning_replay = args.reasoning_replay;
            OpenAiCompatible::new(config)
                .map(HostModel::OpenAi)
                .map_err(|e| format!("--base-url {shown_url}: {e}"))
        }
        Provider::Anthropic => {
            // Manual extended thinking has a documented range; fail here (before the session
            // file exists) with both numbers rather than let the API reject the request.
            if let Thinking::Enabled { budget_tokens } = args.thinking
                && !(MIN_THINKING_BUDGET..args.max_tokens).contains(&budget_tokens)
            {
                return Err(format!(
                    "--thinking enabled:{budget_tokens} is out of range: Anthropic requires the \
                     budget to be >= {MIN_THINKING_BUDGET} and < --max-tokens ({}); interleaved \
                     thinking, which would relax this, is not enabled by this adapter",
                    args.max_tokens
                ));
            }
            let mut config = AnthropicConfig::new(base_url, &args.model);
            if let Some(key) = &args.api_key {
                config = config.with_api_key(agent_provider_anthropic::ApiKey::new(key));
            }
            config.headers = args.headers;
            config.extra_body = args.extra_body;
            config.max_tokens = args.max_tokens;
            if let Some(thinking) = args.thinking.to_config() {
                config = config.with_thinking(thinking);
            }
            Anthropic::new(config)
                .map(HostModel::Anthropic)
                .map_err(|e| format!("--base-url {shown_url}: {e}"))
        }
        Provider::ChatGpt => {
            if !args.chatgpt_oauth {
                // ADR 0026: a built-in descriptor's Responses model
                // authenticated by an API key. The key is the whole credential — no OAuth, no
                // `chatgpt-account-id`, no `OpenAI-Beta`, no `originator` (those belong to the
                // `chatgpt` OAuth route, whose descriptor is the only one that sets them) — and
                // `include`/`store` stay as the adapter builds them.
                let mut config = OpenAiResponsesConfig::new(base_url, &args.model);
                if let Some(key) = &args.api_key {
                    config = config.with_header("Authorization", format!("Bearer {key}"));
                }
                for (name, value) in &args.headers {
                    config = config.with_header(name.clone(), value.clone());
                }
                config.extra_body = args.extra_body;
                config.reasoning_effort = args.reasoning_effort;
                return OpenAiResponses::new(config)
                    .map(HostModel::Responses)
                    .map_err(|e| format!("--base-url {shown_url}: {e}"));
            }
            // No network or file I/O here (ADR 0026): the token and
            // account id resolve fresh on every `complete()` call; only the HTTP client is
            // built once and shared (see `ChatGptModel`).
            let client = agent_provider_openai_responses::build_client(
                agent_provider_openai_responses::DEFAULT_CONNECT_TIMEOUT,
            )
            .map_err(|e| format!("--base-url {shown_url}: {e}"))?;
            Ok(HostModel::ChatGpt(ChatGptModel {
                base_url: args.base_url,
                model: args.model,
                static_headers: args.headers,
                extra_body: args.extra_body,
                reasoning_effort: args.reasoning_effort,
                client,
                endpoints: oauth_chatgpt::Endpoints::default(),
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_only_flags_are_refused_for_openai_compatible() {
        for (thinking, max_tokens) in [
            (Thinking::Adaptive, None),
            (
                Thinking::Enabled {
                    budget_tokens: 8192,
                },
                None,
            ),
            (Thinking::Off, Some(100)),
            (Thinking::Adaptive, Some(100)),
        ] {
            let error = check_provider_flags(Provider::OpenAiCompatible, thinking, max_tokens)
                .expect_err("an Anthropic-only flag under openai-compatible is an error");
            assert!(error.contains("anthropic"), "{error}");
            if thinking != Thinking::Off {
                assert!(error.contains("--thinking"), "{error}");
            }
            if max_tokens.is_some() {
                assert!(error.contains("--max-tokens"), "{error}");
            }
        }
    }

    #[test]
    fn the_default_openai_arguments_are_accepted() {
        check_provider_flags(Provider::OpenAiCompatible, Thinking::Off, None).unwrap();
    }

    #[test]
    fn anthropic_accepts_both_flags() {
        check_provider_flags(
            Provider::Anthropic,
            Thinking::Enabled {
                budget_tokens: 8192,
            },
            Some(16384),
        )
        .unwrap();
    }
}

// ---------- the switchable model (ADR 0026 D5) ----------

/// What a switch needs to build a model for an arbitrary route: the facts that were fixed at
/// startup and do not follow the model. The key is **not** here — it is read per route.
#[derive(Debug, Clone)]
pub struct RouteContext {
    /// The config's `[providers.<name>]` declarations (ADR 0026 D10), so a switch to a named
    /// provider resolves the same way the startup did.
    pub providers: std::collections::BTreeMap<String, crate::ProviderDecl>,
    pub session_id: String,
    /// The user's own headers (generic-descriptor only, `{session_id}` already expanded).
    pub user_headers: Vec<(String, String)>,
    /// Fields merged into every request body — on the startup route and on every later
    /// `/model` route (ADR 0026 D13).
    pub extra_body: Map<String, Value>,
    pub include_usage: Option<bool>,
    pub max_tokens: u32,
    /// The unified thinking level's **name** (ADR 0026 D11); clamped per route at build time.
    pub thinking: String,
    /// A legacy `--thinking enabled:<budget>` the user typed: sent as-is for Anthropic (D11).
    pub thinking_budget: Option<u32>,
}

impl RouteContext {
    /// Resolves one route into model arguments: `resolve_route` for the endpoint and headers,
    /// `auth::resolve_key` for the credential (D2). `api_key` is the pool's chosen key for this
    /// request (D2); `None` uses the entry's first key. A provider without a credential is an
    /// error naming `/login <provider>` (D6), which the caller shows.
    pub fn args_for(
        &self,
        provider_name: &str,
        model: &str,
        api_key: Option<&str>,
    ) -> Result<ModelArgs, String> {
        self.args_for_route(provider_name, model, api_key, None)
    }

    /// [`RouteContext::args_for`] with a route already resolved by a **probe** (ADR 0026 D10):
    /// the adapter, endpoint, headers and thinking map follow `forced` instead of the route's
    /// own resolution. `None` is the ordinary path.
    pub(crate) fn args_for_route(
        &self,
        provider_name: &str,
        model: &str,
        api_key: Option<&str>,
        forced: Option<&crate::provider::NamedModelRoute>,
    ) -> Result<ModelArgs, String> {
        // D10: the provider name resolves through the config's declarations and the auth
        // entry's env, and the key lives under the **name**. A model the catalog maps to an
        // npm Radian cannot speak is refused here, where a switch can still report it.
        let resolved = resolve_route_name(
            provider_name,
            model,
            &self.providers,
            UnsupportedProtocol::Refuse,
        )?;
        let provider_ref = resolved.provider_ref;
        let credential = crate::auth::resolve_key(provider_name).map_err(|e| e.to_string())?;
        // `resolve_key` only ever resolves an `api_key` entry; `chatgpt`'s login is the oauth
        // entry slice ① wrote (ADR 0026), which has no single static key to
        // return here (`credential` stays `None` for it either way — `ChatGptModel` reads
        // `auth.json` itself, per call). `has_login` is the presence check both shapes share.
        if credential.is_none() && api_key.is_none() && !crate::auth::has_login(provider_name) {
            return Err(format!(
                "no credential for `{provider_name}`: run /login {provider_name} (or add an entry \
                 to ~/.radian/credentials/auth.json)"
            ));
        }
        let auth_base_url = credential
            .as_ref()
            .and_then(|credential| credential.env.get(crate::RADIAN_BASE_URL))
            .map(String::as_str);
        // ADR 0026: the wire protocol comes from the catalog (the cache,
        // else the bundled snapshot); a model the catalog does not carry uses the table.
        let catalog = crate::protocol_catalog();
        let (adapter, base_url, headers) = match forced {
            Some(route) => (
                route.protocol.adapter(),
                route.base_url.clone(),
                provider_ref.request_headers(&self.session_id),
            ),
            None => {
                let route = crate::provider::resolve_route_with_protocol(
                    &provider_ref,
                    model,
                    resolved.protocol,
                    None,
                    auth_base_url,
                    &self.user_headers,
                    &self.session_id,
                    Some(&catalog),
                )?;
                (route.adapter(), route.base_url, route.headers)
            }
        };
        // The thinking level is clamped against **this route's** map (ADR 0026 D11): the same
        // level can mean different wire values — or nothing — on different models. A probe
        // uses the map of the protocol it is trying, not the route's old one.
        let thinking_map = match forced {
            Some(route) => crate::thinking_map_for_protocol(route.protocol),
            None => crate::thinking_map_for_ref(&provider_ref, model),
        };
        let (thinking, reasoning_effort) = resolve_thinking_map(
            thinking_map,
            &self.thinking,
            self.thinking_budget,
            adapter,
            Some(self.max_tokens),
        );
        let reasoning_replay = resolve_reasoning_replay(&provider_ref, model);
        // ADR 0018 §2: the chat route sends `min(limit.output, ⌊C/4⌋)` when the catalog knows
        // both. The same number goes into the model and into the switched route's budget
        // (`RouteContext::output_for`), so the sent value and `R` cannot drift.
        let limit = catalog.limit(provider_name, model);
        let max_output_tokens = crate::route_output(adapter, &base_url, self.max_tokens).resolve(
            limit.and_then(|limit| limit.context),
            limit.and_then(|limit| limit.output),
        );
        Ok(ModelArgs {
            provider: adapter,
            base_url,
            model: model.to_owned(),
            api_key: api_key
                .map(str::to_owned)
                .or_else(|| credential.as_ref().map(|credential| credential.key.clone())),
            headers,
            extra_body: self.extra_body.clone(),
            // ADR 0026 D0: the `opencode-go` descriptor asks for streamed usage by default;
            // an explicit `--include-usage` wins either way.
            include_usage: self
                .include_usage
                .or_else(|| crate::default_include_usage(provider_name)),
            max_tokens: self.max_tokens,
            thinking,
            reasoning_effort,
            reasoning_replay,
            max_output_tokens,
            // The `chatgpt` descriptor is the only OAuth Responses route; every other Responses
            // route uses the key from `auth.json` (ADR 0026).
            chatgpt_oauth: provider_name == crate::oauth_chatgpt::PROVIDER_ID,
        })
    }

    /// The output a switched route's request sends (ADR 0018 §2), for the driver's budget
    /// resolver: Anthropic's `max_tokens`, the catalog-derived chat cap, or none. The budget
    /// query uses the route's own protocol and endpoint, so a cross-provider switch does not
    /// inherit the startup route's reservation.
    pub fn output_for(&self, provider_name: &str, model: &str) -> crate::RouteOutput {
        let Ok(route) = resolve_route_name(
            provider_name,
            model,
            &self.providers,
            UnsupportedProtocol::BuiltInTable,
        ) else {
            return crate::RouteOutput::None;
        };
        let auth_base_url = route
            .auth_env
            .as_ref()
            .and_then(|env| env.get(crate::RADIAN_BASE_URL))
            .map(String::as_str);
        let base_url = route
            .provider_ref
            .endpoint_for(model, route.protocol, auth_base_url)
            .unwrap_or_default();
        crate::route_output(route.protocol.adapter(), &base_url, self.max_tokens)
    }
}

/// Where the probe's remembered rule goes (ADR 0026 D10): the user configuration the process
/// read, and whether that read was read-only (a newer `schema_version`).
#[derive(Debug, Clone, Default)]
struct ConfigTarget {
    path: Option<PathBuf>,
    read_only: bool,
}

/// What the probe needs from the route's named-provider resolution (ADR 0026 D10): the
/// provider itself (its `prefer`, `base_urls` and model list), where the current route came
/// from, and the current route. Resolved once per route and cached.
#[derive(Debug, Clone)]
struct ProbePlan {
    named: crate::provider::NamedProvider,
    source: crate::provider::RouteSource,
    current: crate::provider::NamedModelRoute,
}

/// The HTTP statuses that make a named-provider route probe another protocol (ADR 0026 D10).
/// A 401/403, a 429, a 5xx, a transport error and a malformed stream are deliberately absent:
/// they take the normal retry and error paths.
const PROBE_STATUSES: &[u16] = &[400, 404, 405, 415, 422];

/// The probe's status when `error` is a response refusal (ADR 0026 D10): a `Protocol` error
/// that carries one of [`PROBE_STATUSES`]. A context overflow has its own variant and never
/// probes; an error that never saw a response carries no status at all.
fn probe_status(error: &ModelError) -> Option<u16> {
    match error {
        ModelError::Protocol(_) => error
            .http_status()
            .filter(|status| PROBE_STATUSES.contains(status)),
        _ => None,
    }
}

/// One attempt as the failure line names it: `anthropic (HTTP 404)`.
fn attempt_text(protocol: crate::Protocol, error: &ModelError) -> String {
    let detail = match error.http_status() {
        Some(status) => format!("HTTP {status}"),
        None => error.kind().as_str().to_owned(),
    };
    format!("{} ({detail})", protocol.kind())
}

/// `error` with one extra line, for the rule hint and the tried list (ADR 0026 D10). Only the
/// two response variants carry a body a caller can extend; a probe never starts on the others.
fn with_line(error: ModelError, line: String) -> ModelError {
    match error {
        ModelError::Protocol(message) => ModelError::Protocol(format!("{message}\n{line}")),
        ModelError::Unavailable {
            message,
            retry_after,
            status,
        } => ModelError::Unavailable {
            message: format!("{message}\n{line}"),
            retry_after,
            status,
        },
        other => other,
    }
}

/// The route key the probe remembers a working protocol under: the `ConfigChanged` route
/// carries no protocol, so the host keeps the probed protocol for this process (ADR 0026 D10).
fn route_key(route: &ModelRoute) -> String {
    format!("{}/{}", route.provider, route.model)
}

/// The model behind a `/model` switch: every request reads the current route out of the
/// driver's [`ConfigSlot`] and dispatches to the matching adapter, built once per route and
/// cached. The driver only changes the slot between turns (ADR 0004 path B), so one request
/// never sees a route change mid-turn.
pub struct SwitchableModel {
    slot: Arc<ConfigSlot>,
    initial_route: ModelRoute,
    ctx: RouteContext,
    cache: Mutex<HashMap<String, Arc<HostModel>>>,
    /// Where a successful probe appends its rule (ADR 0026 D10).
    config_target: ConfigTarget,
    /// The queue a successful probe writes its `ConfigChanged` route into, once
    /// `open_session` has wired it. `None` in a bare test, which still probes.
    config_queue: Option<Arc<ConfigQueue>>,
    /// The named-provider resolution behind each route, or `None` when the route is not an
    /// eligible named-provider route. Resolved once per route.
    probe_plans: Mutex<HashMap<String, Option<ProbePlan>>>,
    /// Working protocols a probe remembered, keyed by [`route_key`]. A route with an entry is
    /// never probed again: the remembered protocol is what every later request uses.
    probed: Mutex<HashMap<String, crate::provider::NamedModelRoute>>,
}

impl SwitchableModel {
    /// `initial` is the model built at startup for `initial_route`.
    pub fn new(
        initial: HostModel,
        initial_route: ModelRoute,
        slot: Arc<ConfigSlot>,
        ctx: RouteContext,
    ) -> Self {
        // The startup model was built with the entry's first key, so its cache entry carries
        // that key's fingerprint; without it the first request would rebuild the model.
        let fingerprint = crate::key_pool::pool(&initial_route.provider)
            .and_then(|pool| pool.keys().first().map(PoolKey::fingerprint))
            .unwrap_or_default();
        let cache = Mutex::new(HashMap::from([(
            Self::cache_key(&initial_route, &ctx.thinking, &fingerprint),
            Arc::new(initial),
        )]));
        Self {
            slot,
            initial_route,
            ctx,
            cache,
            config_target: ConfigTarget::default(),
            config_queue: None,
            probe_plans: Mutex::new(HashMap::new()),
            probed: Mutex::new(HashMap::new()),
        }
    }

    /// The file a successful probe appends its rule to, and the reading process's read-only
    /// verdict (ADR 0026 D10). Without this call a probe still runs, but the notice says the
    /// rule was not saved.
    pub fn with_config(mut self, path: Option<PathBuf>, read_only: bool) -> Self {
        self.config_target = ConfigTarget { path, read_only };
        self
    }

    /// The queue the driver drains at the next turn boundary. A successful probe queues the
    /// same route value a `/model` switch writes, so the journal records the applied route
    /// (ADR 0026 D10). `open_session` sets this once the queue exists.
    pub fn set_config_queue(&mut self, queue: Arc<ConfigQueue>) {
        self.config_queue = Some(queue);
    }

    /// The cache key: the route, the thinking level in force (ADR 0026 D11), and the chosen
    /// key's fingerprint (D2). A `/thinking` change or a key switch must rebuild the model
    /// even though the route is unchanged; without the fingerprint the second key would never
    /// be built.
    fn cache_key(route: &ModelRoute, thinking: &str, fingerprint: &str) -> String {
        format!(
            "{provider}/{model}#{thinking}#{fingerprint}",
            provider = route.provider,
            model = route.model
        )
    }

    /// The route in force right now: the slot's model change, else the startup route.
    pub fn current_route(&self) -> ModelRoute {
        ModelRoute::from_value(&self.slot.value()).unwrap_or_else(|| self.initial_route.clone())
    }

    /// The thinking level in force: the slot's `thinking` change (D11), else the startup one.
    pub fn current_thinking(&self) -> String {
        self.slot
            .value()
            .get("thinking")
            .and_then(|level| level.as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| self.ctx.thinking.clone())
    }

    /// The probe's view of `route`, or `None` when the route is not a named-provider route.
    /// Cached: the resolution reads `auth.json` and the model-list cache.
    fn probe_plan(&self, route: &ModelRoute) -> Option<ProbePlan> {
        let key = route_key(route);
        if let Some(plan) = self
            .probe_plans
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&key)
        {
            return plan.clone();
        }
        let plan = self.resolve_probe_plan(route);
        self.probe_plans
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(key, plan.clone());
        plan
    }

    /// Resolves the named provider behind `route` (ADR 0026 D10). A built-in descriptor, a
    /// provider without a credential and any resolution failure yield `None`: there is nothing
    /// to probe and the ordinary error path is untouched.
    fn resolve_probe_plan(&self, route: &ModelRoute) -> Option<ProbePlan> {
        let resolved = resolve_route_name(
            &route.provider,
            &route.model,
            &self.ctx.providers,
            UnsupportedProtocol::BuiltInTable,
        )
        .ok()?;
        let crate::ProviderRef::Named(named) = &resolved.provider_ref else {
            return None;
        };
        let current = named.resolve_model(&route.model);
        Some(ProbePlan {
            named: named.clone(),
            source: current.source.clone(),
            current,
        })
    }

    /// The model for `route`, `thinking` and the chosen `api_key`, built and cached on first
    /// use. The `Arc` comes out of the lock, so the lock is never held across the request. A
    /// route a probe already resolved uses the remembered protocol (ADR 0026 D10).
    fn model_for(
        &self,
        route: &ModelRoute,
        thinking: &str,
        api_key: Option<&str>,
        fingerprint: &str,
    ) -> Result<Arc<HostModel>, String> {
        let forced = self
            .probed
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&route_key(route))
            .cloned();
        let key = Self::cache_key(route, thinking, fingerprint);
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(model) = cache.get(&key) {
            return Ok(model.clone());
        }
        let ctx = RouteContext {
            thinking: thinking.to_owned(),
            ..self.ctx.clone()
        };
        let args = ctx.args_for_route(&route.provider, &route.model, api_key, forced.as_ref())?;
        let model = build_model(args).map(Arc::new)?;
        cache.insert(key, model.clone());
        Ok(model)
    }

    /// Probes the other protocols for `route` (ADR 0026 D10) and returns the first success.
    /// On success it remembers the route, queues the `ConfigChanged` and saves the rule. When
    /// every attempt fails it returns `first` with the tried list and the rule hint appended.
    #[allow(clippy::too_many_arguments)] // the request's own state, threaded without a struct
    async fn probe(
        &self,
        route: &ModelRoute,
        plan: &ProbePlan,
        first: ModelError,
        first_status: u16,
        request: ModelRequest,
        api_key: Option<&str>,
        thinking: &str,
        fingerprint: &str,
        cancellation: &radian_core::CancellationToken,
        events: &dyn radian_core::EventSink,
    ) -> Result<ModelResponse, ModelError> {
        let failed = plan.current.protocol;
        let mut tried = vec![attempt_text(failed, &first)];
        for protocol in plan.named.probe_candidates(&route.model, failed) {
            let forced = plan.named.probe_route(&route.model, protocol);
            let built = self
                .ctx
                .args_for_route(&route.provider, &route.model, api_key, Some(&forced))
                .and_then(build_model)
                .map(Arc::new);
            let model = match built {
                Ok(model) => model,
                Err(message) => {
                    // Building the attempt failed before it reached the provider. It still
                    // counts as a tried protocol, with the message as its reason.
                    let error = ModelError::Protocol(message);
                    tried.push(attempt_text(protocol, &error));
                    continue;
                }
            };
            let result = model
                .complete(
                    request.clone(),
                    ModelContext {
                        cancellation: cancellation.clone(),
                        events,
                    },
                )
                .await;
            match result {
                Ok(response) => {
                    self.remember(route, thinking, fingerprint, forced, model);
                    let saved = self.save_rule(&plan.named.name, &route.model, protocol);
                    crate::key_pool::push_notice(format!(
                        "{}/{}: {} refused (HTTP {}); {} works; {}",
                        plan.named.name,
                        route.model,
                        failed.kind(),
                        first_status,
                        protocol.kind(),
                        saved
                    ));
                    return Ok(response);
                }
                Err(error) => tried.push(attempt_text(protocol, &error)),
            }
        }
        let hint = format!(
            "tried {}. Set the protocol: [[providers.{}.models]] match = \"{}\" kind = \"<{}>\"",
            tried.join(", "),
            plan.named.name,
            route.model,
            crate::provider::PROVIDER_KINDS.join("|")
        );
        Err(with_line(first, hint))
    }

    /// Remembers a probe's working route (in memory for later requests, and by the rule on
    /// disk) and queues the same `ConfigChanged` a `/model` switch writes.
    fn remember(
        &self,
        route: &ModelRoute,
        thinking: &str,
        fingerprint: &str,
        forced: crate::provider::NamedModelRoute,
        model: Arc<HostModel>,
    ) {
        self.probed
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(route_key(route), forced);
        self.cache
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(Self::cache_key(route, thinking, fingerprint), model);
        if let Some(queue) = &self.config_queue {
            let current = ModelRoute::new(&route.provider, &route.model);
            // A `/model` switch queued mid-turn is the user's decision and lands first; queueing
            // this (old) route after it would silently undo the switch. Only record the route
            // when nothing else will replace it.
            match queue.pending_model_route() {
                Some(pending) if pending != current => {}
                _ => queue.push(current.to_value()),
            }
        }
    }

    /// Appends the remembered rule and returns the notice's last clause (ADR 0026 D10):
    /// `saved to <path>`, or `not saved: <reason>`.
    fn save_rule(&self, provider: &str, model: &str, protocol: crate::Protocol) -> String {
        let Some(path) = &self.config_target.path else {
            return "not saved: no configuration path is known".to_owned();
        };
        match crate::config::providers_write::write_protocol_rule(
            path,
            self.config_target.read_only,
            provider,
            model,
            protocol.kind(),
        ) {
            Ok(outcome) => format!("saved to {}", outcome.path().display()),
            Err(error) => format!("not saved: {error}"),
        }
    }
}

impl Model for SwitchableModel {
    async fn complete(
        &self,
        request: ModelRequest,
        cx: ModelContext<'_>,
    ) -> Result<ModelResponse, ModelError> {
        // The route was read before the lock, so a switch that lands mid-request cannot swap
        // this call's model (D5).
        let route = self.current_route();
        let thinking = self.current_thinking();
        // ADR 0026 D2: one pool per provider, process state only. The picked key is sticky
        // until it rests; a switch queues one notice and changes the cache key, so the next
        // request builds the next key's model.
        let pool = crate::key_pool::pool(&route.provider);
        let picked = pool.as_ref().and_then(|pool| pool.pick());
        if let Some(notice) = picked.as_ref().and_then(|picked| picked.notice.clone()) {
            crate::key_pool::push_notice(notice);
        }
        let api_key = picked.as_ref().map(|picked| picked.key.clone());
        let fingerprint = picked
            .as_ref()
            .map(|picked| picked.fingerprint.clone())
            .unwrap_or_default();
        let model = self
            .model_for(&route, &thinking, api_key.as_deref(), &fingerprint)
            .map_err(ModelError::Protocol)?;

        // ADR 0026 D10: the probe plan is resolved before the request so the same call can
        // retry the other protocols. The request is cloned only when a probe may run.
        let plan = self.probe_plan(&route);
        let already_probed = self
            .probed
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains_key(&route_key(&route));
        let source = plan.as_ref().map(|plan| plan.source.clone());
        let probe_allowed = !already_probed
            && matches!(
                source,
                Some(crate::provider::RouteSource::ModelList | crate::provider::RouteSource::Kind)
            );
        let retry_request = probe_allowed.then(|| request.clone());
        let rule_pattern = match &source {
            Some(crate::provider::RouteSource::Rule(pattern)) if !already_probed => {
                Some(pattern.clone())
            }
            _ => None,
        };

        let ModelContext {
            cancellation,
            events,
        } = cx;
        let result = model
            .complete(
                request,
                ModelContext {
                    cancellation: cancellation.clone(),
                    events,
                },
            )
            .await;
        let error = match result {
            Ok(response) => return Ok(response),
            Err(error) => error,
        };
        // ADR 0026 D10: a rule that fails with a probe status names itself, so the user can fix
        // or delete it. It never probes: the rule is the decision.
        if let Some(pattern) = rule_pattern
            && probe_status(&error).is_some()
        {
            let mut line = format!("rule {pattern}");
            if let Some(path) = &self.config_target.path {
                line.push_str(&format!(" in {}", path.display()));
            }
            return Err(with_line(error, line));
        }
        let Some(status) = probe_status(&error) else {
            self.rest_key(pool.as_ref(), picked.as_ref(), &error);
            return Err(error);
        };
        let Some(plan) = plan.filter(|_| probe_allowed) else {
            self.rest_key(pool.as_ref(), picked.as_ref(), &error);
            return Err(error);
        };
        let Some(request) = retry_request else {
            return Err(error);
        };
        let result = self
            .probe(
                &route,
                &plan,
                error,
                status,
                request,
                api_key.as_deref(),
                &thinking,
                &fingerprint,
                &cancellation,
                events,
            )
            .await;
        if let Err(error) = &result {
            self.rest_key(pool.as_ref(), picked.as_ref(), error);
        }
        result
    }
}

impl SwitchableModel {
    /// ADR 0026 D2: only a quota 429 rests the key the request used.
    fn rest_key(
        &self,
        pool: Option<&Arc<crate::key_pool::KeyPool>>,
        picked: Option<&crate::key_pool::PickedKey>,
        error: &ModelError,
    ) {
        if let (Some(pool), Some(picked)) = (pool, picked) {
            pool.rest_for_error(picked.index, error);
        }
    }
}

#[cfg(test)]
mod thinking_resolution_tests {
    use super::*;

    /// ADR 0026 D11: a model with no map anywhere gets **no** parameter — neither an
    /// Anthropic `thinking` setting nor an OpenAI-compatible `reasoning_effort`. This is the
    /// guard for the "no invented wire values" rule.
    #[test]
    fn a_mapless_model_resolves_to_nothing() {
        let (thinking, effort) = resolve_thinking(
            "opencode-go",
            "gpt-5-nano",
            "high",
            Provider::OpenAiCompatible,
            None,
        );
        assert_eq!(thinking, Thinking::Off);
        assert_eq!(effort, None);
        let (thinking, effort) = resolve_thinking(
            "opencode-go",
            "gpt-5-nano",
            "high",
            Provider::Anthropic,
            None,
        );
        assert_eq!(thinking, Thinking::Off);
        assert_eq!(effort, None);
    }

    /// The verified deepseek map resolves a level into the OpenAI-side wire value, and an
    /// Anthropic route from the same level into its own setting.
    #[test]
    fn a_mapped_model_resolves_per_adapter() {
        let (_, effort) = resolve_thinking(
            "opencode-go",
            "deepseek-v4.1-flash",
            "medium",
            Provider::OpenAiCompatible,
            None,
        );
        assert_eq!(effort.as_deref(), Some("high"), "the clamp goes up");
        let (thinking, effort) = resolve_thinking(
            "opencode-go",
            "deepseek-v4.1-flash",
            "medium",
            Provider::Anthropic,
            None,
        );
        assert_eq!(effort, None);
        assert_eq!(
            thinking,
            Thinking::Off,
            "a wire value the anthropic adapter does not know sends nothing"
        );
    }
}

#[cfg(test)]
mod compaction_settings_tests {
    use super::*;
    use std::collections::BTreeMap;

    fn builtin(name: &str) -> crate::ProviderRef {
        crate::resolve_provider(name, &BTreeMap::new(), None)
            .expect("a built-in provider resolves")
            .0
    }

    /// `"current"` means "do not override": the summary call carries no setting and there is
    /// nothing to say (ADR 0018).
    #[test]
    fn current_carries_no_setting() {
        let provider = builtin("opencode-go");
        let (settings, notice) = compaction_settings_for(
            &provider,
            "deepseek-v4.1-flash",
            COMPACTION_THINKING_CURRENT,
            4096,
        );
        assert_eq!(settings, serde_json::Value::Null);
        assert_eq!(notice, None);
    }

    /// An OpenAI-compatible route gets `reasoning_effort`; a supported level produces no notice.
    #[test]
    fn an_openai_route_converts_the_level_to_reasoning_effort() {
        let provider = builtin("opencode-go");
        let (settings, notice) =
            compaction_settings_for(&provider, "deepseek-v4.1-flash", "low", 4096);
        assert_eq!(settings, serde_json::json!({"reasoning_effort": "low"}));
        assert_eq!(notice, None, "low is supported, nothing to say");
    }

    /// A level the route does not support clamps with the `/thinking` rule (up first) and is
    /// said once. deepseek-v4.1-flash supports low/high/max, so `medium` clamps up to `high`.
    #[test]
    fn an_unsupported_level_is_clamped_and_noticed() {
        let provider = builtin("opencode-go");
        let (settings, notice) =
            compaction_settings_for(&provider, "deepseek-v4.1-flash", "medium", 4096);
        assert_eq!(settings, serde_json::json!({"reasoning_effort": "high"}));
        let notice = notice.expect("a clamp is said once");
        assert!(notice.contains("medium → high"), "{notice}");
        assert!(notice.contains("low/high/max"), "{notice}");
    }

    /// An Anthropic route gets the adapter's `thinking` shape; `off` sends nothing.
    #[test]
    fn an_anthropic_route_converts_the_level_to_a_thinking_object() {
        let provider = builtin("anthropic");
        let (settings, notice) = compaction_settings_for(&provider, "claude-test", "low", 4096);
        assert_eq!(
            settings,
            serde_json::json!({"thinking": {"type": "enabled", "budget_tokens": 2048}})
        );
        assert_eq!(notice, None);
        let (settings, notice) = compaction_settings_for(&provider, "claude-test", "off", 4096);
        assert_eq!(settings, serde_json::Value::Null, "off sends no key");
        assert_eq!(notice, None, "off is supported on this route");
    }

    /// A route with no thinking map carries nothing and says nothing (there is no level to
    /// clamp to; the summary call matches the conversation, ADR 0026 D11).
    #[test]
    fn a_mapless_route_carries_nothing_and_says_nothing() {
        let provider = builtin("opencode-go");
        let (settings, notice) = compaction_settings_for(&provider, "gpt-5-nano", "low", 4096);
        assert_eq!(settings, serde_json::Value::Null);
        assert_eq!(notice, None);
    }
}

#[cfg(test)]
mod probe_tests {
    use super::*;

    /// ADR 0026 D10: only a response refusal with one of the five statuses probes. A context
    /// overflow and a stream error (no HTTP prefix) do not; a plain 400 does.
    #[test]
    fn only_the_protocol_refusal_statuses_probe() {
        for status in PROBE_STATUSES {
            let error = ModelError::Protocol(format!("HTTP {status} Bad Request: no"));
            assert_eq!(probe_status(&error), Some(*status), "{status}");
        }
        for status in [401, 402, 403, 408, 409, 413, 418, 429, 500, 503] {
            let error = ModelError::Protocol(format!("HTTP {status}: no"));
            assert_eq!(probe_status(&error), None, "{status}");
        }
        assert_eq!(
            probe_status(&ModelError::ContextOverflow(
                "HTTP 400: prompt is too long".into()
            )),
            None,
            "an overflow has its own path"
        );
        assert_eq!(
            probe_status(&ModelError::Protocol("unreadable stream event".into())),
            None
        );
        assert_eq!(
            probe_status(&ModelError::Unavailable {
                message: "HTTP 500: busy".into(),
                retry_after: None,
                status: Some(500),
            }),
            None
        );
        assert_eq!(probe_status(&ModelError::Transport("down".into())), None);
        assert_eq!(probe_status(&ModelError::EmptyResponse), None);
    }

    /// The failure line names each attempt the way the ADR does.
    #[test]
    fn the_attempt_text_names_the_protocol_and_the_status() {
        assert_eq!(
            attempt_text(
                crate::Protocol::OpenAiCompatible,
                &ModelError::Protocol("HTTP 400 Bad Request: no".into())
            ),
            "openai-compatible (HTTP 400)"
        );
        assert_eq!(
            attempt_text(
                crate::Protocol::Anthropic,
                &ModelError::Transport("down".into())
            ),
            "anthropic (transport)"
        );
    }
}

#[cfg(test)]
mod thinking_clamp_tests {
    use super::*;

    /// Review B1: a **level's** own budget is clamped to the request's `max_tokens - 1`
    /// instead of failing the model build.
    #[test]
    fn a_level_budget_is_clamped_to_max_tokens() {
        let (thinking, _) = resolve_thinking(
            "anthropic",
            "claude-test",
            "max",
            Provider::Anthropic,
            Some(4096),
        );
        assert_eq!(
            thinking,
            Thinking::Enabled {
                budget_tokens: 4095
            }
        );
    }

    /// With `max_tokens <= 1024` no `enabled` level fits, so the clamp walks past them: the
    /// request carries no `enabled` setting.
    #[test]
    fn no_enabled_level_fits_when_max_tokens_is_tiny() {
        for level in ["minimal", "low", "high", "xhigh", "max"] {
            let (thinking, _) = resolve_thinking(
                "anthropic",
                "claude-test",
                level,
                Provider::Anthropic,
                Some(1024),
            );
            assert_ne!(
                thinking,
                Thinking::Enabled {
                    budget_tokens: 1024
                },
                "{level} must not send an enabled budget"
            );
            assert!(
                !matches!(thinking, Thinking::Enabled { .. }),
                "{level} must not send enabled: {thinking:?}"
            );
        }
        // `adaptive` (medium) and `off` still fit: they have no budget.
        let (thinking, _) = resolve_thinking(
            "anthropic",
            "claude-test",
            "medium",
            Provider::Anthropic,
            Some(1024),
        );
        assert_eq!(thinking, Thinking::Adaptive);
    }

    /// The user's explicit legacy budget is **not** clamped: it stays a usage error (B1.3).
    #[test]
    fn a_legacy_budget_is_not_clamped() {
        let (thinking, _) = resolve_thinking_map(
            Some(crate::provider::ANTHROPIC_THINKING),
            "high",
            Some(999_999),
            Provider::Anthropic,
            Some(4096),
        );
        assert_eq!(
            thinking,
            Thinking::Enabled {
                budget_tokens: 999_999
            }
        );
    }

    /// The completion hides a level only when no budget can fit at all (B1.2): with room to
    /// clamp, every level stays selectable.
    #[test]
    fn the_completion_hides_levels_that_cannot_fit() {
        assert_eq!(
            crate::supported_thinking_levels_with("anthropic", "claude-test", Some(4096)),
            vec!["off", "minimal", "low", "medium", "high", "xhigh", "max"],
            "every budget clamps into 4096"
        );
        assert_eq!(
            crate::supported_thinking_levels_with("anthropic", "claude-test", Some(1024)),
            vec!["off", "medium"],
            "only the budget-less levels fit"
        );
    }
}
