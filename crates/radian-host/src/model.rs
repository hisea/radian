//! The shared model construction for both hosts: one [`HostModel`] over the two provider
//! adapters, and one [`build_model`] from resolved arguments.
//!
//! `agent_core::Model::complete` returns `impl Future` (RPITIT), so `Model` is **not object
//! safe**: a host cannot hold a `Box<dyn Model>`. An enum that implements `Model` by matching
//! on its variants keeps static dispatch and lets either provider live behind one type.
//!
//! The host still owns its flags, its key lookup and its secrets; this module only turns the
//! resolved values into a model, so the two hosts cannot build providers differently.

use std::str::FromStr;

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

    /// The environment variable read when `--api-key-env` is omitted.
    pub fn default_api_key_env(self) -> &'static str {
        match self {
            Self::OpenAiCompatible => "OPENAI_API_KEY",
            Self::Anthropic => "ANTHROPIC_API_KEY",
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
    /// `anthropic` only.
    pub thinking: Thinking,
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
