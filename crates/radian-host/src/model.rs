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
}

impl Provider {
    /// The name written into the session's config snapshot (never the key or a header).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiCompatible => "openai-compatible",
            Self::Anthropic => "anthropic",
        }
    }

    /// The public endpoint used when `--base-url` is omitted. For Anthropic this is
    /// everything before `/v1/messages`.
    pub fn default_base_url(self) -> &'static str {
        match self {
            Self::OpenAiCompatible => "https://api.openai.com/v1",
            Self::Anthropic => "https://api.anthropic.com",
        }
    }

    /// Parses the `--provider` value.
    pub fn from_name(name: &str) -> Result<Self, String> {
        match name {
            "openai-compatible" => Ok(Self::OpenAiCompatible),
            "anthropic" => Ok(Self::Anthropic),
            other => Err(format!(
                "--provider {other:?} is not one of openai-compatible, anthropic"
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
}

/// Either provider behind one `Model`. `Model` is not object safe, so this is an enum and not
/// a `Box<dyn Model>`.
pub enum HostModel {
    OpenAi(OpenAiCompatible),
    Anthropic(Anthropic),
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
        }
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
) -> (Thinking, Option<String>) {
    let map = crate::descriptor(descriptor_id)
        .and_then(|descriptor| crate::thinking_map_for(descriptor, model));
    match crate::clamp_thinking(map, level) {
        Some((_, wire)) if adapter == Provider::OpenAiCompatible => {
            (Thinking::Off, Some(wire.to_owned()))
        }
        Some((_, wire)) => (thinking_from_wire(wire), None),
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
            .unwrap_or(Thinking::Adaptive),
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
    pub session_id: String,
    /// The user's own headers (generic-descriptor only, `{session_id}` already expanded).
    pub user_headers: Vec<(String, String)>,
    pub include_usage: Option<bool>,
    pub max_tokens: u32,
    /// The unified thinking level's **name** (ADR 0026 D11); clamped per route at build time.
    pub thinking: String,
}

impl RouteContext {
    /// Resolves one route into model arguments: `resolve_route` for the endpoint and headers,
    /// `auth::resolve_key` for the credential (D2). A provider without a credential is an error
    /// naming `/login <provider>` (D6), which the caller shows.
    pub fn args_for(&self, provider_id: &str, model: &str) -> Result<ModelArgs, String> {
        let credential = crate::auth::resolve_key(provider_id).map_err(|e| e.to_string())?;
        if credential.is_none() {
            return Err(format!(
                "no credential for `{provider_id}`: run /login {provider_id} (or add an entry to \
                 ~/.radian/credentials/auth.json)"
            ));
        }
        let auth_base_url = credential
            .as_ref()
            .and_then(|credential| credential.env.get(crate::RADIAN_BASE_URL))
            .map(String::as_str);
        let route = crate::resolve_route(
            provider_id,
            model,
            None,
            auth_base_url,
            &self.user_headers,
            &self.session_id,
        )?;
        // The thinking level is clamped against **this route's** map (ADR 0026 D11): the same
        // level can mean different wire values — or nothing — on different models.
        let (thinking, reasoning_effort) =
            resolve_thinking(provider_id, model, &self.thinking, route.adapter());
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
    cache: std::sync::Mutex<HashMap<ModelRoute, Arc<HostModel>>>,
}

impl SwitchableModel {
    /// `initial` is the model built at startup for `initial_route`.
    pub fn new(
        initial: HostModel,
        initial_route: ModelRoute,
        slot: Arc<ConfigSlot>,
        ctx: RouteContext,
    ) -> Self {
        let cache =
            std::sync::Mutex::new(HashMap::from([(initial_route.clone(), Arc::new(initial))]));
        Self {
            slot,
            initial_route,
            ctx,
            cache,
        }
    }

    /// The route in force right now: the slot's model change, else the startup route.
    pub fn current_route(&self) -> ModelRoute {
        ModelRoute::from_value(&self.slot.value()).unwrap_or_else(|| self.initial_route.clone())
    }

    /// The model for the current route, built and cached on first use. The `Arc` comes out of
    /// the lock, so the lock is never held across the request.
    fn current(&self) -> Result<Arc<HostModel>, String> {
        let route = self.current_route();
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(model) = cache.get(&route) {
            return Ok(model.clone());
        }
        let args = self.ctx.args_for(&route.provider, &route.model)?;
        let model = build_model(args).map(Arc::new)?;
        cache.insert(route, model.clone());
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
