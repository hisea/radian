//! The shared model construction for both hosts: one [`HostModel`] over the two provider
//! adapters, and one [`build_model`] from resolved arguments.
//!
//! `agent_core::Model::complete` returns `impl Future` (RPITIT), so `Model` is **not object
//! safe**: a host cannot hold a `Box<dyn Model>`. An enum that implements `Model` by matching
//! on its variants keeps static dispatch and lets either provider live behind one type.
//!
//! The host still owns its flags, its key lookup and its secrets; this module only turns the
//! resolved values into a model, so the two hosts cannot build providers differently.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use radian_runtime::{ConfigSlot, ModelRoute};

use agent_core::{Model, ModelContext, ModelError, ModelRequest, ModelResponse};
use agent_provider_anthropic::{Anthropic, AnthropicConfig, ThinkingConfig};
use agent_provider_openai_compatible::{OpenAiCompatible, OpenAiCompatibleConfig};
use agent_provider_openai_responses::{OpenAiResponses, OpenAiResponsesConfig};

use crate::oauth_chatgpt;

/// The `max_tokens` sent to Anthropic when `--max-tokens` is not given. The Messages API
/// requires the field and defines no server-side default, so the host must pick one. It
/// counts **thinking tokens and the answer together** (wire notes §2.3), so a thinking turn
/// needs room for both: 4096 truncated thinking coding answers, and 16384 is within the output
/// ceiling of the current Claude models. Older models are set smaller with `--max-tokens`.
/// Overridable.
pub const DEFAULT_MAX_TOKENS: u32 = 16384;

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
    /// `openai-compatible` only (`--include-usage`).
    pub include_usage: Option<bool>,
    /// `anthropic` only.
    pub max_tokens: u32,
    /// `anthropic` only: the resolved thinking setting.
    pub thinking: Thinking,
    /// `openai-compatible` only: the thinking level's wire value (ADR 0026 D11), sent as
    /// `reasoning_effort`. `None` sends no key.
    pub reasoning_effort: Option<String>,
    /// `openai-compatible` only: the assistant-message field the model requires its reasoning
    /// replayed under (ADR 0014 Q1). `None` sends no reasoning.
    pub reasoning_replay: Option<String>,
}

/// Either provider behind one `Model`. `Model` is not object safe, so this is an enum and not
/// a `Box<dyn Model>`.
pub enum HostModel {
    OpenAi(OpenAiCompatible),
    Anthropic(Anthropic),
    ChatGpt(ChatGptModel),
}

impl Model for HostModel {
    async fn complete(
        &self,
        mut request: ModelRequest,
        cx: ModelContext<'_>,
    ) -> Result<ModelResponse, ModelError> {
        strip_renamed_tool_aliases(&mut request);
        match self {
            Self::OpenAi(model) => model.complete(request, cx).await,
            Self::Anthropic(model) => model.complete(request, cx).await,
            Self::ChatGpt(model) => model.complete(request, cx).await,
        }
    }
}

/// Removes the one-version alias specs (ADR 0043 decision 3) from every request, so the model
/// is never offered `git_status` or `memory_get`.
///
/// The aliases stay in the provider's `tool_specs()` because `agent_core`'s preflight refuses a
/// call whose name is not registered **before** any host hook or the provider runs, and a call
/// to an old name must still reach the provider (which answers it and says it was renamed). This
/// is the only boundary at which "registered but not advertised" can be expressed without an
/// `agent-core` API change.
fn strip_renamed_tool_aliases(request: &mut ModelRequest) {
    request
        .tools
        .retain(|spec| radian_permissions::canonical_tool_name(&spec.name) == spec.name);
}

/// The `chatgpt` descriptor's model (ADR 0026). Unlike the other two
/// variants, this does **not** wrap a single pre-built adapter: the access token can expire
/// mid-session, so every `complete()` call resolves a fresh token (refreshing it first if
/// it is within [`oauth_chatgpt::REFRESH_MARGIN`] of expiring — slice ①'s `ensure_fresh`),
/// decodes the account id from it (slice ①'s `decode_account_id`, also done fresh each call,
/// per ADR 0026: it is never stored), and only then builds the
/// short-lived [`OpenAiResponses`] adapter that actually sends the request.
///
/// Building that adapter is cheap (URL parsing and a `reqwest::Client::builder()`, no I/O —
/// see `OpenAiResponses::new`), so doing it per call costs nothing a long-lived instance
/// would have saved; the alternative (mutating a cached adapter's headers in place) is not
/// available, since the adapter stores them in an already-built, immutable `HeaderMap`.
pub struct ChatGptModel {
    /// Everything before `/responses` (the same convention [`OpenAiResponsesConfig::base_url`]
    /// documents). Overridable so a test points it at a local mock; production always uses
    /// the descriptor's fixed endpoint.
    pub base_url: String,
    pub model: String,
    /// The descriptor's static headers only (`OpenAI-Beta`, `originator`, `session_id` —
    /// `chatgpt_headers` in `provider.rs`), **not** `Authorization`/`chatgpt-account-id`.
    pub static_headers: Vec<(String, String)>,
    pub reasoning_effort: Option<String>,
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
        config.reasoning_effort = self.reasoning_effort.clone();
        let model = OpenAiResponses::new(config).map_err(|error| {
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
/// (ADR 0018, decision 1).
pub const COMPACTION_THINKING_CURRENT: &str = "current";

/// Builds the request-level settings the **compaction** (summary) call carries, from the
/// configured `[compaction] thinking` level (ADR 0018, decision 1). Returns
/// `(settings, notice)`:
///
/// * `settings` is the object for `agent_core::AgentConfig::compaction_settings` — the wire
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
    match crate::resolve_provider(provider_name, providers, None) {
        Ok((provider, _)) => compaction_settings_for(&provider, model, level, max_tokens).0,
        Err(_) => serde_json::Value::Null,
    }
}

/// The reasoning-replay field for a route (ADR 0014 Q2): the `[providers.<id>]`
/// override when the provider is named, else the catalog's `interleaved` field. `None` means
/// the model does not require replay, or declares a field Radian does not replay (e.g.
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

/// Whether a route replays reasoning, resolved by provider **name** (ADR 0014 Q2):
/// the `[providers.<name>]` override, else the catalog's `interleaved` field. This is what the
/// per-route budget resolver uses, so the estimate counts the reasoning the request really
/// carries. A name reachable only through `auth.json`'s `env` has no declaration here and no
/// catalog entry, so it does not replay.
pub fn route_replays_reasoning(
    provider_name: &str,
    model: &str,
    providers: &std::collections::BTreeMap<String, crate::ProviderDecl>,
) -> bool {
    match crate::resolve_provider(provider_name, providers, None) {
        Ok((provider_ref, _)) => resolve_reasoning_replay(&provider_ref, model).is_some(),
        Err(_) => crate::catalog::catalog_reasoning_replay(provider_name, model).is_some(),
    }
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
            config.include_usage = args.include_usage;
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
            config.max_tokens = args.max_tokens;
            if let Some(thinking) = args.thinking.to_config() {
                config = config.with_thinking(thinking);
            }
            Anthropic::new(config)
                .map(HostModel::Anthropic)
                .map_err(|e| format!("--base-url {shown_url}: {e}"))
        }
        Provider::ChatGpt => {
            // No network or file I/O here (ADR 0026): the access token
            // and account id are resolved fresh on every `complete()` call, never at build
            // time — see `ChatGptModel`'s doc comment for why that is the right place and
            // costs nothing extra.
            Ok(HostModel::ChatGpt(ChatGptModel {
                base_url: args.base_url,
                model: args.model,
                static_headers: args.headers,
                reasoning_effort: args.reasoning_effort,
                endpoints: oauth_chatgpt::Endpoints::default(),
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR 0043 decision 3: the renamed tools' old names are never advertised, whatever the
    /// provider. The aliases stay registered in the provider so Core's preflight accepts the
    /// call.
    #[test]
    fn the_renamed_tool_aliases_are_stripped_from_every_request() {
        let spec = |name: &str| agent_core::ToolSpec {
            name: name.to_owned(),
            description: String::new(),
            input_schema: serde_json::json!({"type": "object"}),
            concurrency: agent_core::ToolConcurrency::Shared,
        };
        let mut request = ModelRequest {
            system_prompt: None,
            messages: Vec::new(),
            tools: vec![
                spec("read"),
                spec("git_status"),
                spec("git_diff"),
                spec("memory_get"),
                spec("memory_search"),
            ],
            settings: serde_json::json!({}),
        };
        strip_renamed_tool_aliases(&mut request);
        let names: Vec<&str> = request
            .tools
            .iter()
            .map(|spec| spec.name.as_str())
            .collect();
        assert_eq!(names, ["read", "git_diff", "memory_search"]);
    }

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
    pub include_usage: Option<bool>,
    pub max_tokens: u32,
    /// The unified thinking level's **name** (ADR 0026 D11); clamped per route at build time.
    pub thinking: String,
    /// A legacy `--thinking enabled:<budget>` the user typed: sent as-is for Anthropic (D11).
    pub thinking_budget: Option<u32>,
}

impl RouteContext {
    /// Resolves one route into model arguments: `resolve_route` for the endpoint and headers,
    /// `auth::resolve_key` for the credential (D2). A provider without a credential is an error
    /// naming `/login <provider>` (D6), which the caller shows.
    pub fn args_for(&self, provider_name: &str, model: &str) -> Result<ModelArgs, String> {
        // D10: the provider name resolves through the config's declarations and the auth
        // entry's env, and the key lives under the **name**.
        let auth_env = crate::AuthStore::open()
            .ok()
            .and_then(|store| store.entry_env(provider_name));
        let (provider_ref, _warning) =
            crate::resolve_provider(provider_name, &self.providers, auth_env.as_ref())?;
        let credential = crate::auth::resolve_key(provider_name).map_err(|e| e.to_string())?;
        // `resolve_key` only ever resolves an `api_key` entry; `chatgpt`'s login is the oauth
        // entry slice ① wrote (ADR 0026), which has no single static key to
        // return here (`credential` stays `None` for it either way — `ChatGptModel` reads
        // `auth.json` itself, per call). `has_login` is the presence check both shapes share.
        if credential.is_none() && !crate::auth::has_login(provider_name) {
            return Err(format!(
                "no credential for `{provider_name}`: run /login {provider_name} (or add an entry \
                 to ~/.radian/credentials/auth.json)"
            ));
        }
        let auth_base_url = credential
            .as_ref()
            .and_then(|credential| credential.env.get(crate::RADIAN_BASE_URL))
            .map(String::as_str);
        let route = crate::resolve_route_for(
            &provider_ref,
            model,
            None,
            auth_base_url,
            &self.user_headers,
            &self.session_id,
        )?;
        // The thinking level is clamped against **this route's** map (ADR 0026 D11): the same
        // level can mean different wire values — or nothing — on different models.
        let (thinking, reasoning_effort) = resolve_thinking_ref(
            &provider_ref,
            model,
            &self.thinking,
            self.thinking_budget,
            route.adapter(),
            Some(self.max_tokens),
        );
        let reasoning_replay = resolve_reasoning_replay(&provider_ref, model);
        Ok(ModelArgs {
            provider: route.adapter(),
            base_url: route.base_url,
            model: model.to_owned(),
            api_key: credential.as_ref().map(|credential| credential.key.clone()),
            headers: route.headers,
            include_usage: self.include_usage,
            max_tokens: self.max_tokens,
            thinking,
            reasoning_effort,
            reasoning_replay,
        })
    }
}

/// The model behind a `/model` switch: every request reads the current route out of the
/// driver's [`ConfigSlot`] and dispatches to the matching adapter, built once per route and
/// cached. The driver only changes the slot between turns (ADR 0004 path B), so one request
/// never sees a route change mid-turn.
pub struct SwitchableModel {
    slot: Arc<ConfigSlot>,
    initial_route: ModelRoute,
    ctx: RouteContext,
    cache: std::sync::Mutex<HashMap<String, Arc<HostModel>>>,
}

impl SwitchableModel {
    /// `initial` is the model built at startup for `initial_route`.
    pub fn new(
        initial: HostModel,
        initial_route: ModelRoute,
        slot: Arc<ConfigSlot>,
        ctx: RouteContext,
    ) -> Self {
        let cache = std::sync::Mutex::new(HashMap::from([(
            Self::cache_key(&initial_route, &ctx.thinking),
            Arc::new(initial),
        )]));
        Self {
            slot,
            initial_route,
            ctx,
            cache,
        }
    }

    /// The cache key: the route **and** the thinking level in force (ADR 0026 D11) — a
    /// `/thinking` change must rebuild the model even though the route is unchanged.
    fn cache_key(route: &ModelRoute, thinking: &str) -> String {
        format!("{}/{model}#{thinking}", route.provider, model = route.model)
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

    /// The model for the current route and thinking, built and cached on first use. The `Arc`
    /// comes out of the lock, so the lock is never held across the request.
    fn current(&self) -> Result<Arc<HostModel>, String> {
        let route = self.current_route();
        let thinking = self.current_thinking();
        let key = Self::cache_key(&route, &thinking);
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(model) = cache.get(&key) {
            return Ok(model.clone());
        }
        let ctx = RouteContext {
            thinking: thinking.clone(),
            ..self.ctx.clone()
        };
        let args = ctx.args_for(&route.provider, &route.model)?;
        let model = build_model(args).map(Arc::new)?;
        cache.insert(key, model.clone());
        Ok(model)
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
        let model = self.current().map_err(ModelError::Protocol)?;
        model.complete(request, cx).await
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
    /// nothing to say (ADR 0018, decision 1).
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
