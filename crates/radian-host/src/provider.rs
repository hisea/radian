//! Provider descriptors (ADR 0026 D0): one registered entry per provider, saying which wire
//! protocol each model uses, what base URL to talk to, how to log in, and which request
//! headers to add.
//!
//! The adapters (`agent-provider-*`) do not change: a descriptor only chooses which adapter to
//! build and what to pass it. The four built-ins are registered at compile time; there is no
//! dynamic plugin loading (v5 §7, ADR 0023 U8).

use std::collections::BTreeMap;

use crate::catalog::ModelLimit;
use crate::headers_usage::HeadersUsage;
use crate::model::Provider;
use crate::usage::{UsageArgs, UsageFetcher};

/// Which wire protocol a model talks, mapped to one of the three existing adapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    OpenAiCompatible,
    Anthropic,
    /// OpenAI's Responses API (`agent-provider-openai-responses`, ADR 0026 slice
    /// 2): today only the `chatgpt` descriptor uses it.
    Responses,
}

impl Protocol {
    /// The `--provider` name this protocol maps to ([`Provider::from_name`]).
    pub fn adapter_name(self) -> &'static str {
        match self {
            Self::OpenAiCompatible => "openai-compatible",
            Self::Anthropic => "anthropic",
            Self::Responses => "openai-responses",
        }
    }

    pub fn adapter(self) -> Provider {
        match self {
            Self::OpenAiCompatible => Provider::OpenAiCompatible,
            Self::Anthropic => Provider::Anthropic,
            Self::Responses => Provider::ChatGpt,
        }
    }

    /// The models.dev `npm` this protocol is selected by (ADR 0026):
    /// `@ai-sdk/openai` is the Responses API since AI SDK 5. Any other value is unsupported and
    /// the model is refused rather than guessed at.
    pub fn from_npm(npm: &str) -> Option<Self> {
        match npm {
            "@ai-sdk/openai-compatible" => Some(Self::OpenAiCompatible),
            "@ai-sdk/anthropic" => Some(Self::Anthropic),
            "@ai-sdk/openai" => Some(Self::Responses),
            _ => None,
        }
    }
}

/// The refusal a built-in descriptor's model gets when the catalog names an `npm` Radian has no
/// adapter for (ADR 0026): explicit, never a silent fallback to chat.
pub fn unsupported_api_message(npm: &str) -> String {
    format!("radian does not support this model's API ({npm})")
}

/// How a provider is logged into (ADR 0026 D3). Only [`LoginMethod::ApiKey`] is implemented in
/// this build; the other two are placeholders the interface leaves room for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginMethod {
    /// The user pastes an API key. `extra_fields` names the additional values a descriptor
    /// wants, stored in the entry's `env` object (e.g. the generic `openai-compatible` base
    /// URL under [`crate::auth::RADIAN_BASE_URL`]).
    ApiKey {
        extra_fields: &'static [&'static str],
    },
    /// PKCE + local-loopback browser OAuth. Not implemented.
    OAuthBrowser,
    /// RFC 8628 device code. Not implemented.
    DeviceCode,
}

/// One model in a descriptor's built-in list. `protocol` is the **fallback** the model uses
/// when the catalog has no entry for it (ADR 0026); the catalog's `npm`
/// decides the protocol whenever the catalog carries the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DescriptorModel {
    pub id: &'static str,
    pub name: &'static str,
    pub protocol: Protocol,
    /// The window numbers this descriptor's own upstream table carries, when it carries any:
    /// the `contextWindow` / `maxTokens` pair of the `openai-codex` entry in
    /// `@oh-my-pi/pi-catalog/src/models.json` for `chatgpt`. [`ModelLimit::default()`] (all
    /// three unknown) for every descriptor whose numbers come from models.dev (ADR 0018): the
    /// resolver uses this only when the catalog has no entry for the route
    /// ([`crate::catalog::resolve`]), never to override a catalog number.
    pub limit: ModelLimit,
    /// The model's `thinkingLevelMap` (ADR 0026 D11): level → wire value, a `None` wire value
    /// is pi's `null` (unsupported). Empty = no model-level map; the provider's applies.
    pub thinking: ThinkingMap,
}

/// How a provider's usage is obtained (ADR 0027). The optional field
/// `ProviderDescriptor::usage` names it; a provider without one draws no `usage` row and is
/// never polled.
#[derive(Clone, Copy)]
pub enum UsageSource {
    /// Poll an endpoint: the fetch builds the provider's request from the facts the session
    /// hands it, sends it, and parses the answer into a `UsageSnapshot`.
    Endpoint(EndpointUsage),
    /// Parse every model response's rate-limit headers into a `UsageSnapshot` (slice ④): no
    /// polling, no key, nothing stored. The header names live on the [`HeadersUsage`] value.
    Headers(HeadersUsage),
}

/// One `Endpoint` usage source (ADR 0027).
#[derive(Clone, Copy)]
pub struct EndpointUsage {
    /// The endpoint's default URL.
    pub url: &'static str,
    /// The item ids an earlier version used for this provider's usage row. They stay valid as
    /// aliases of the `usage` component for one version ([`usage_item_alias`]).
    pub legacy_item_ids: &'static [&'static str],
    /// Builds this provider's fetch from the session's own facts. The credential stays in the
    /// process: it is handed to the fetch and never reaches a journal, a log line or a notice.
    pub build: fn(&UsageArgs) -> Box<dyn UsageFetcher>,
}

/// The `Endpoint` usage source a provider's descriptor declares, or `None` (ADR 0027).
/// The TUI spawns a refresh only for a provider with one; a `Headers`
/// provider has no endpoint to poll.
pub fn usage_endpoint(provider: &str) -> Option<&'static EndpointUsage> {
    match descriptor(provider)?.usage.as_ref()? {
        UsageSource::Endpoint(endpoint) => Some(endpoint),
        UsageSource::Headers(_) => None,
    }
}

/// Whether a provider's descriptor declares a usage source: the `usage` footer component's and
/// `/usage`'s gate. A provider without one draws no row, never a fabricated empty one (ADR 0027
/// ).
pub fn has_usage_source(provider: &str) -> bool {
    descriptor(provider).is_some_and(|entry| entry.usage.is_some())
}

/// The current id of an item id an earlier version used, or `None` when `id` is not a legacy
/// usage id (`opencode-go.usage`, `chatgpt.usage` — ADR 0027). The TUI
/// accepts such an id for one version and warns once.
pub fn usage_item_alias(id: &str) -> Option<&'static str> {
    descriptors()
        .iter()
        .any(|entry| {
            matches!(&entry.usage,
                Some(UsageSource::Endpoint(endpoint)) if endpoint.legacy_item_ids.contains(&id))
        })
        .then_some(crate::usage::USAGE_ITEM)
}

/// One registered provider.
pub struct ProviderDescriptor {
    pub id: &'static str,
    pub display_name: &'static str,
    pub default_base_url: &'static str,
    pub login: LoginMethod,
    /// The provider-level thinking map (ADR 0026 D11), used when a model has none.
    pub thinking: ThinkingMap,
    /// The protocol a model uses when the built-in list does not name it. Uniform for
    /// `anthropic`/`openai`/`openai-compatible`; `opencode-go` defaults to openai-compatible
    /// (its Anthropic models are all listed).
    pub default_protocol: Protocol,
    /// The verification request `/login` sends for this provider (ADR 0026 D3): when `Some`,
    /// one `GET` of this absolute URL with `Authorization: Bearer <key>` — the usage footer's
    /// own auth — instead of the protocol's `GET <base>/models`. `opencode-go` names its usage
    /// endpoint: its chat base serves no `/models` path. `None` keeps
    /// `<base>/models`.
    pub verification_url: Option<&'static str>,
    /// The model-list URL the background refresh GETs (ADR 0026 D0): when `Some`, this absolute
    /// URL instead of `<base>/models`. `opencode-go` names its discovery endpoint: its chat
    /// base serves no `/models`. `None` keeps `<base>/models`.
    pub models_url: Option<&'static str>,
    /// The built-in model list (the full list is later merged with the cache and `/v1/models`,
    /// ADR 0026 D0).
    pub models: &'static [DescriptorModel],
    /// Extra request headers, expanded with the process's session id. See
    /// [`ProviderDescriptor::request_headers`].
    pub headers: fn(&str) -> Vec<(String, String)>,
    /// Where this provider's usage comes from (ADR 0027), or `None`: the
    /// footer's `usage` row then has nothing to draw for it.
    pub usage: Option<UsageSource>,
}

impl ProviderDescriptor {
    /// The request headers for one request, with `{session_id}` expanded. For `opencode-go`
    /// this is byte-identical to `--header x-opencode-session={session_id}` plus the client
    /// header (ADR 0026 D0).
    pub fn request_headers(&self, session_id: &str) -> Vec<(String, String)> {
        (self.headers)(session_id)
    }

    /// The built-in entry for `model`, if the descriptor lists it.
    pub fn model(&self, model: &str) -> Option<&'static DescriptorModel> {
        self.models.iter().find(|entry| entry.id == model)
    }

    /// The protocol `model` uses **when the catalog cannot answer** (ADR 0026): the
    /// built-in entry's protocol, or the descriptor's default for a model the list does not
    /// name (always known, never `None`). [`ProviderRef::resolve_protocol`] consults the catalog
    /// first and falls back to this.
    pub fn protocol_for(&self, model: &str) -> Protocol {
        self.model(model)
            .map_or(self.default_protocol, |entry| entry.protocol)
    }

    /// The window numbers the built-in entry itself carries for `model` (see
    /// [`DescriptorModel::limit`]), or `None` when it carries none: the model is not in the
    /// list, or the descriptor's numbers come from models.dev. The resolver's fallback for a
    /// provider models.dev does not carry (`chatgpt`) is this.
    pub fn limit_for(&self, model: &str) -> Option<ModelLimit> {
        let limit = self.model(model)?.limit;
        (limit.context.is_some() || limit.input.is_some() || limit.output.is_some())
            .then_some(limit)
    }
}

/// The `auth.json` `env` key naming a named provider's wire protocol (ADR 0026 D10). pi
/// ignores it; Radian reads it when `[providers.<name>]` is absent. The default is the
/// generic `openai-compatible`.
pub const RADIAN_KIND: &str = "RADIAN_KIND";

impl Protocol {
    /// The wire protocol a `kind` names (D10: a named/custom provider may only declare one of
    /// these two; `Responses` is deliberately absent — it is the `chatgpt` descriptor's fixed
    /// protocol, not a dial a custom endpoint can choose, since its base URL shape and its
    /// headers come from the OAuth login, not from a user-typed endpoint).
    pub fn from_kind(kind: &str) -> Option<Self> {
        match kind {
            "openai-compatible" => Some(Self::OpenAiCompatible),
            "anthropic" => Some(Self::Anthropic),
            _ => None,
        }
    }

    /// The `kind` spelling this protocol is declared with.
    pub fn kind(self) -> &'static str {
        self.adapter_name()
    }
}

/// The `/login` choice for "not one of the built-ins" — a named provider is born there
/// (ADR 0026 D10). The TUI wizard and `radian login` share it so they cannot drift.
pub const CUSTOM_LOGIN: &str = "custom";

/// The `/login` choice for the ChatGPT-subscription device-code flow (ADR 0026
/// decision 3). The browser flow's id is [`crate::oauth_chatgpt::PROVIDER_ID`] (`"chatgpt"`)
/// — both store into the same `auth.json` entry, so there is no separate constant for it.
pub const CHATGPT_DEVICE_LOGIN: &str = "chatgpt-device";

/// The `/login` provider-choice candidates: the built-in descriptors, the two ChatGPT OAuth
/// choices (ADR 0026), then [`CUSTOM_LOGIN`]. The TUI wizard's own `choices()`
/// mirrors this list (same ids, same order) so the picker and `radian login`'s numbered list
/// never drift.
pub fn login_choices() -> Vec<(String, String)> {
    // `chatgpt` is excluded here: it is a real descriptor (so `/model` lists its models), but
    // its *login* is the two OAuth entries pushed below, not the generic "ask for an API key"
    // flow every other descriptor gets from this loop — including it too would list it twice.
    let mut choices: Vec<(String, String)> = descriptors()
        .iter()
        .filter(|d| d.id != crate::oauth_chatgpt::PROVIDER_ID)
        .map(|d| (d.id.to_owned(), d.display_name.to_owned()))
        .collect();
    choices.push((
        crate::oauth_chatgpt::PROVIDER_ID.to_owned(),
        "Sign in with ChatGPT (opens your browser)".to_owned(),
    ));
    choices.push((
        CHATGPT_DEVICE_LOGIN.to_owned(),
        "ChatGPT (device code; no browser needed on this machine)".to_owned(),
    ));
    choices.push((
        CUSTOM_LOGIN.to_owned(),
        "a custom endpoint: name, protocol, base URL and key".to_owned(),
    ));
    choices
}

/// The characters a provider name may use (ADR 0026 D10: letters, digits, `-`, `_`).
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Why `name` cannot declare a **named** provider (`[providers.<name>]` or a `/login`
/// custom entry). `None` when the name is fine.
pub fn name_error(name: &str) -> Option<String> {
    if !valid_name(name) {
        Some(format!(
            "`{name}` is not a valid provider name (letters, digits, `-` and `_` only)"
        ))
    } else if name == GENERIC_ID {
        Some(format!(
            "`{GENERIC_ID}` is reserved for the built-in generic provider; log in with \
             `/login` and pick it directly"
        ))
    } else if descriptor(name).is_some() {
        Some(format!(
            "`{name}` is a built-in provider; a named provider must not shadow it"
        ))
    } else {
        None
    }
}

/// One named provider: a custom service the user gave a name (ADR 0026 D10). Its key is
/// stored under that name in `auth.json`, so several custom endpoints each have their own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedProvider {
    pub name: String,
    pub protocol: Protocol,
    pub base_url: String,
    /// The `[providers.<name>]` override for the catalog's reasoning replay (ADR 0014
    ///  Q2). `None` falls through to the catalog.
    pub reasoning_replay: Option<ReasoningReplayOverride>,
}

/// A `[providers.<id>]` override for the catalog's reasoning replay (ADR 0014 R5):
/// `reasoning_replay = "<field>"` or `reasoning_replay = false`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReasoningReplayOverride {
    /// Replay under this field, whatever the catalog says.
    Field(String),
    /// Never replay, whatever the catalog says.
    Off,
}

/// Which side declared a named provider (D10: config wins, both is a warning).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamedSource {
    /// `[providers.<name>]` in `config.toml`.
    Config,
    /// The `auth.json` entry's `env` (`RADIAN_BASE_URL`, `RADIAN_KIND`).
    Auth,
}

/// One `[providers.<name>]` declaration's fields, as the config reader carries them (ADR 0026
/// D10). The TUI/CLI resolve them through [`resolve_provider`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderDecl {
    pub kind: String,
    pub base_url: String,
    /// `reasoning_replay` (ADR 0014 R5): a field name, or `false` to switch replay
    /// off. `None` lets the catalog decide.
    pub reasoning_replay: Option<ReasoningReplayOverride>,
}

/// A provider named on the command line or in the config: a built-in descriptor, or a named
/// one (ADR 0026 D10). The descriptor struct holds function pointers, so equality and `Debug`
/// are by id, not by value.
#[derive(Clone)]
pub enum ProviderRef {
    BuiltIn(&'static ProviderDescriptor),
    Named(NamedProvider),
}

impl PartialEq for ProviderRef {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::BuiltIn(a), Self::BuiltIn(b)) => a.id == b.id,
            (Self::Named(a), Self::Named(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for ProviderRef {}

impl std::fmt::Debug for ProviderRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BuiltIn(descriptor) => write!(f, "BuiltIn({})", descriptor.id),
            Self::Named(named) => write!(f, "Named({named:?})"),
        }
    }
}

impl ProviderRef {
    /// The name every user-facing surface (`/model`, notices) shows.
    pub fn id(&self) -> &str {
        match self {
            Self::BuiltIn(descriptor) => descriptor.id,
            Self::Named(named) => &named.name,
        }
    }

    /// The `auth.json` entry this provider's key lives under: the descriptor id for built-ins,
    /// the name for named ones. This is D10's fix: every custom endpoint gets its own key
    /// instead of sharing the generic descriptor's slot.
    pub fn key_id(&self) -> &str {
        match self {
            Self::BuiltIn(descriptor) => descriptor.id,
            Self::Named(named) => &named.name,
        }
    }

    /// The wire protocol the built-in table (or a named provider's `kind`) gives `model`,
    /// **ignoring the catalog** (ADR 0026): the fallback [`Self::resolve_protocol`]
    /// uses for a model the catalog does not carry, and what a route's notice compares against.
    pub fn protocol_for(&self, model: &str) -> Protocol {
        match self {
            Self::BuiltIn(descriptor) => descriptor.protocol_for(model),
            Self::Named(named) => named.protocol,
        }
    }

    /// The wire protocol of `model` (ADR 0026): a built-in descriptor's
    /// model takes it from the catalog entry's `npm` when the catalog carries the model, and
    /// from the built-in table otherwise; an `npm` Radian has no adapter for is refused. A named
    /// provider is untouched (D10/decision 5): its declared `kind` decides.
    pub fn resolve_protocol(
        &self,
        catalog: Option<&crate::Catalog>,
        model: &str,
    ) -> Result<Protocol, String> {
        match self {
            Self::Named(named) => Ok(named.protocol),
            // The generic descriptor names no built-in models: the user typed the id and the
            // endpoint, so the catalog's `npm` must not dictate its protocol (ADR 0026
            // 's spirit — only a descriptor's own model list is gated).
            Self::BuiltIn(descriptor) if descriptor.id == GENERIC_ID => {
                Ok(descriptor.protocol_for(model))
            }
            Self::BuiltIn(descriptor) => {
                match catalog.and_then(|catalog| catalog.npm(descriptor.id, model)) {
                    Some(npm) => {
                        Protocol::from_npm(npm).ok_or_else(|| unsupported_api_message(npm))
                    }
                    None => Ok(descriptor.protocol_for(model)),
                }
            }
        }
    }

    /// The endpoint `model` talks to (ADR 0026): the auth entry's base URL
    /// when it carries one, else the hard-coded per-(descriptor, protocol) base; a named
    /// provider has exactly one endpoint. The catalog's own `api` field is never used.
    pub fn endpoint_for(
        &self,
        protocol: Protocol,
        auth_base_url: Option<&str>,
    ) -> Result<String, String> {
        match self {
            Self::BuiltIn(descriptor) => {
                if let Some(base) = auth_base_url {
                    return Ok(base.to_owned());
                }
                descriptor_base_url(descriptor.id, protocol)
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        format!(
                            "radian has no {} endpoint for provider `{}`",
                            protocol.adapter_name(),
                            descriptor.id
                        )
                    })
            }
            Self::Named(named) => Ok(named.base_url.clone()),
        }
    }

    /// The request headers for one request (`{session_id}` expanded). A named provider has
    /// none of its own.
    pub fn request_headers(&self, session_id: &str) -> Vec<(String, String)> {
        match self {
            Self::BuiltIn(descriptor) => descriptor.request_headers(session_id),
            Self::Named(_) => Vec::new(),
        }
    }

    /// Whether `--base-url`/`--header` and config `model.base_url`/`model.headers` apply:
    /// only the generic descriptor lets the user choose the endpoint (ADR 0026 D0); a named
    /// provider's endpoint is its declaration.
    pub fn takes_base_url_override(&self) -> bool {
        matches!(self, Self::BuiltIn(d) if d.id == GENERIC_ID)
    }
}

/// Resolves a provider **name** (`--provider`, config `model.provider`, `/model`) into a
/// [`ProviderRef`] (ADR 0026 D10). A built-in id is the descriptor; anything else is looked
/// up in the config's `[providers.<name>]` first, then the `auth.json` entry's `env`
/// (`RADIAN_BASE_URL`, `RADIAN_KIND`, default `openai-compatible`). Both sources defining the
/// same name is a one-line warning, config wins; neither is an error naming the built-ins.
/// `openai-compatible` itself always resolves to the built-in descriptor (a reserved name).
pub fn resolve_provider(
    name: &str,
    config_entries: &BTreeMap<String, ProviderDecl>,
    auth_env: Option<&std::collections::BTreeMap<String, String>>,
) -> Result<(ProviderRef, Option<String>), String> {
    if name == GENERIC_ID {
        return Ok((ProviderRef::BuiltIn(descriptor(GENERIC_ID).unwrap()), None));
    }
    if let Some(decl) = config_entries.get(name) {
        let protocol = Protocol::from_kind(&decl.kind).ok_or_else(|| {
            format!(
                "providers.{name}.kind = {:?} is not one of openai-compatible|anthropic",
                decl.kind
            )
        })?;
        let warning = auth_env.filter(|_| descriptor(name).is_none()).map(|_| {
            format!(
                "provider `{name}` is declared in both config and auth.json; the config \
                     wins"
            )
        });
        return Ok((
            ProviderRef::Named(NamedProvider {
                name: name.to_owned(),
                protocol,
                base_url: decl.base_url.clone(),
                reasoning_replay: decl.reasoning_replay.clone(),
            }),
            warning,
        ));
    }
    if let Some(descriptor) = descriptor(name) {
        return Ok((ProviderRef::BuiltIn(descriptor), None));
    }
    if let Some(env) = auth_env {
        let kind = env
            .get(RADIAN_KIND)
            .map(String::as_str)
            .unwrap_or("openai-compatible");
        let protocol = Protocol::from_kind(kind).ok_or_else(|| {
            format!(
                "provider `{name}`: auth.json env.{RADIAN_KIND} = {kind:?} is not one of \
                 openai-compatible|anthropic"
            )
        })?;
        let base_url = env.get(crate::auth::RADIAN_BASE_URL).ok_or_else(|| {
            format!(
                "provider `{name}` is not built in and names no endpoint: add \
                 `[providers.{name}]` with `base_url`, or set env.RADIAN_BASE_URL in its \
                 auth.json entry"
            )
        })?;
        return Ok((
            ProviderRef::Named(NamedProvider {
                name: name.to_owned(),
                protocol,
                base_url: base_url.clone(),
                reasoning_replay: None,
            }),
            None,
        ));
    }
    Err(format!(
        "unknown provider {name:?}: choose one of {}, or declare it under [providers.{name}] \
         or in auth.json",
        descriptors()
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// `opencode-go` adds the session-routing header and a client identifier (ADR 0026 D0). The
/// session header's value is the session id, exactly as `{session_id}` expands.
fn opencode_go_headers(session_id: &str) -> Vec<(String, String)> {
    vec![
        ("x-opencode-session".to_owned(), session_id.to_owned()),
        ("x-opencode-client".to_owned(), "radian".to_owned()),
    ]
}

fn no_headers(_session_id: &str) -> Vec<(String, String)> {
    Vec::new()
}

/// `chatgpt`'s **static** headers only (ADR 0026): `OpenAI-Beta` and
/// `originator` are fixed, and `session_id` is this request's session id (names verified
/// against `@oh-my-pi/pi-catalog/src/wire/codex.ts`'s `OPENAI_HEADERS`, 2026-10-16, read-only
/// reference — `session_id` is that literal lowercase-with-underscore spelling, not a
/// `x-`-prefixed header). `Authorization` and `chatgpt-account-id` are **not** here: they need
/// a possibly-refreshed access token, which this function (a plain fn pointer, called
/// synchronously from route resolution) cannot fetch. [`crate::model::ChatGptModel::complete`]
/// adds them per call.
fn chatgpt_headers(session_id: &str) -> Vec<(String, String)> {
    vec![
        (
            "OpenAI-Beta".to_owned(),
            "responses=experimental".to_owned(),
        ),
        (
            "originator".to_owned(),
            crate::oauth_chatgpt::ORIGINATOR.to_owned(),
        ),
        ("session_id".to_owned(), session_id.to_owned()),
    ]
}

/// The base URL `opencode-go` serves its OpenAI **chat** and **Responses** protocols on (ADR
/// 0026 revision 2 decision 2): everything before `/chat/completions` / `/responses`. This is
/// models.dev's `api` value for the provider — but radian writes it here and never reads it.
const OPENCODE_GO_OPENAI_BASE: &str = "https://opencode.ai/inference/go/openai/v1";
/// The base URL `opencode-go` serves Anthropic Messages on: everything before `/v1/messages`,
/// which is what the Anthropic adapter appends (`agent-provider-anthropic/src/model.rs:73`). So
/// models.dev's `…/anthropic/v1` value is **not** used verbatim — the adapter's own convention
/// would double the `/v1`. Verified by reading that file; not by a live request.
const OPENCODE_GO_ANTHROPIC_BASE: &str = "https://opencode.ai/inference/go/anthropic";
/// The URL `opencode-go` lists its models on (ADR 0026 D0): the chat base serves no `/models`
/// ; the zen endpoint answers the OpenAI list shape
/// (`{"object":"list","data":[{"id":…}]}`, lead 2026-10-20).
const OPENCODE_GO_MODELS_URL: &str = "https://opencode.ai/zen/go/v1/models";
const ANTHROPIC_BASE: &str = "https://api.anthropic.com";
const OPENAI_BASE: &str = "https://api.openai.com/v1";

/// The `thinkingLevelMap` shared by grok-4.7, grok-4.6 (upstream `models.json`,
/// commit cdd8a70): **low, medium, high, xhigh** are supported, every other level is `null`.
const THINKING_UP_TO_XHIGH: ThinkingMap = &[
    ("off", None),
    ("minimal", None),
    ("low", Some("low")),
    ("medium", Some("medium")),
    ("high", Some("high")),
    ("xhigh", Some("xhigh")),
    ("max", None),
];

/// The `thinkingLevelMap` shared by glm-5.3-flash, deepseek-v4.1-flash, deepseek-v4-flash-vision-exp, glm-5.3, deepseek-v4-flash (upstream `models.json`,
/// commit cdd8a70): **low, high, max** are supported, every other level is `null`.
const THINKING_LOW_HIGH_MAX: ThinkingMap = &[
    ("off", None),
    ("minimal", None),
    ("low", Some("low")),
    ("medium", None),
    ("high", Some("high")),
    ("xhigh", None),
    ("max", Some("max")),
];

/// The `thinkingLevelMap` shared by qwen3.8-max, qwen3.8-flash (upstream `models.json`,
/// commit cdd8a70): **low, medium, xhigh** are supported, every other level is `null`.
const THINKING_LOW_MED_XHIGH: ThinkingMap = &[
    ("off", None),
    ("minimal", None),
    ("low", Some("low")),
    ("medium", Some("medium")),
    ("high", None),
    ("xhigh", Some("xhigh")),
    ("max", None),
];

/// The `thinkingLevelMap` shared by kimi-k3 (upstream `models.json`,
/// commit cdd8a70): **max** are supported, every other level is `null`.
const THINKING_MAX_ONLY: ThinkingMap = &[
    ("off", None),
    ("minimal", None),
    ("low", None),
    ("medium", None),
    ("high", None),
    ("xhigh", None),
    ("max", Some("max")),
];

/// The `thinkingLevelMap` shared by cat-jam-free, space-bunny, gpt-5.6-luna, claude-haiku-5-5, gpt-6-luna (upstream `models.json`,
/// commit cdd8a70): **low, medium, high, xhigh, max** are supported, every other level is `null`.
const THINKING_LOW_TO_MAX: ThinkingMap = &[
    ("off", None),
    ("minimal", None),
    ("low", Some("low")),
    ("medium", Some("medium")),
    ("high", Some("high")),
    ("xhigh", Some("xhigh")),
    ("max", Some("max")),
];

/// The `thinkingLevelMap` shared by glm-5.2, deepseek-v4-pro (upstream `models.json`,
/// commit cdd8a70): **high, max** are supported, every other level is `null`.
const THINKING_HIGH_MAX: ThinkingMap = &[
    ("off", None),
    ("minimal", None),
    ("low", None),
    ("medium", None),
    ("high", Some("high")),
    ("xhigh", None),
    ("max", Some("max")),
];

/// The `thinkingLevelMap` shared by hy3 (upstream `models.json`,
/// commit cdd8a70): **low, high** are supported, every other level is `null`.
const THINKING_LOW_HIGH: ThinkingMap = &[
    ("off", None),
    ("minimal", None),
    ("low", Some("low")),
    ("medium", None),
    ("high", Some("high")),
    ("xhigh", None),
    ("max", None),
];

/// The `thinkingLevelMap` shared by muse-spark-1.2-contributor, muse-spark-1.3-contributor (upstream `models.json`,
/// commit cdd8a70): **minimal, low, medium, high, xhigh** are supported, every other level is `null`.
const THINKING_MINIMAL_TO_XHIGH: ThinkingMap = &[
    ("off", None),
    ("minimal", Some("minimal")),
    ("low", Some("low")),
    ("medium", Some("medium")),
    ("high", Some("high")),
    ("xhigh", Some("xhigh")),
    ("max", None),
];

/// The `thinkingLevelMap` shared by hy4-preview (upstream `models.json`,
/// commit cdd8a70): **high** are supported, every other level is `null`.
const THINKING_HIGH_ONLY: ThinkingMap = &[
    ("off", None),
    ("minimal", None),
    ("low", None),
    ("medium", None),
    ("high", Some("high")),
    ("xhigh", None),
    ("max", None),
];

/// The `thinkingLevelMap` shared by step-5-preview-free (upstream `models.json`,
/// commit cdd8a70): **low, medium, high** are supported, every other level is `null`.
const THINKING_LOW_MED_HIGH: ThinkingMap = &[
    ("off", None),
    ("minimal", None),
    ("low", Some("low")),
    ("medium", Some("medium")),
    ("high", Some("high")),
    ("xhigh", None),
    ("max", None),
];

/// `opencode-go`'s built-in list, resynced to the **models.dev** snapshot of 2026-10-17
/// (ADR 0026): the model ids and their wire protocols are models.dev's,
/// the `thinkingLevelMap`s are `monotykamary/pi-opencode-go-provider@cdd8a70` `models.json`
/// (accessed 2026-10-14), which models.dev does not carry. Directory and table disagreeing is
/// resolved in the catalog's favour at run time, so a row here is only the fallback for a model
/// the catalog does not carry.
///
/// Measured against the snapshot: `@ai-sdk/openai-compatible` (20, the provider default) →
/// [`Protocol::OpenAiCompatible`]; `@ai-sdk/anthropic` (6) → [`Protocol::Anthropic`];
/// `@ai-sdk/openai` (5: grok-4.7, gpt-5.6-luna, gpt-6-luna, muse-spark-1.2/1.3-contributor) →
/// [`Protocol::Responses`]. `beam-free` and `step-5-preview` were added; `deepseek-v4-flash`
/// and `grok-4.6` are gone from the snapshot and dropped here;
/// `gemini-4-argon-contributor` (`@ai-sdk/google`) is deliberately **not** listed — when the
/// catalog carries it, the route is refused with [`unsupported_api_message`] rather than sent
/// as chat.
///
/// Structural test: `crates/radian-host/tests/opencode_go_models.rs`.
const OPENCODE_GO_MODELS: &[DescriptorModel] = &[
    go_openai("mimo-v2.6-pro", "MiMo-V2.6-Pro"),
    go_openai("mimo-v2.5", "MiMo V2.5"),
    DescriptorModel {
        thinking: THINKING_UP_TO_XHIGH,
        ..go_responses("grok-4.7", "Grok 4.7")
    },
    go_openai("longcat-2.5-preview-free", "LongCat 2.5 Preview Free"),
    DescriptorModel {
        thinking: THINKING_LOW_HIGH_MAX,
        ..go_openai("glm-5.3-flash", "GLM-5.3-Flash")
    },
    DescriptorModel {
        thinking: THINKING_LOW_MED_XHIGH,
        ..go_anthropic("qwen3.8-max", "Qwen3.8 Max")
    },
    DescriptorModel {
        thinking: THINKING_MAX_ONLY,
        ..go_openai("kimi-k3", "Kimi K3")
    },
    DescriptorModel {
        thinking: THINKING_LOW_HIGH_MAX,
        ..go_openai("deepseek-v4.1-flash", "DeepSeek V4.1 Flash")
    },
    DescriptorModel {
        thinking: THINKING_LOW_HIGH_MAX,
        ..go_openai(
            "deepseek-v4-flash-vision-exp",
            "DeepSeek V4 Flash Vision Exp",
        )
    },
    go_openai("longcat-2.0", "LongCat-2.0"),
    DescriptorModel {
        thinking: THINKING_LOW_TO_MAX,
        ..go_openai("cat-jam-free", "Cat Jam Free")
    },
    DescriptorModel {
        thinking: THINKING_LOW_TO_MAX,
        ..go_openai("space-bunny", "Space Bunny")
    },
    go_anthropic("minimax-m2.7", "MiniMax-M2.7"),
    go_openai("mimo-v2.5-pro", "MiMo V2.5 Pro"),
    go_openai("mimo-v2.6-flash", "MiMo-V2.6-Flash"),
    go_anthropic("minimax-m3", "MiniMax-M3"),
    DescriptorModel {
        thinking: THINKING_LOW_TO_MAX,
        ..go_responses("gpt-5.6-luna", "GPT-5.6 Luna")
    },
    DescriptorModel {
        thinking: THINKING_LOW_MED_XHIGH,
        ..go_anthropic("qwen3.8-flash", "Qwen3.8 Flash")
    },
    DescriptorModel {
        thinking: THINKING_HIGH_MAX,
        ..go_openai("glm-5.2", "GLM-5.2")
    },
    DescriptorModel {
        thinking: THINKING_LOW_HIGH,
        ..go_openai("hy3", "Hy3")
    },
    DescriptorModel {
        thinking: THINKING_MINIMAL_TO_XHIGH,
        ..go_responses("muse-spark-1.2-contributor", "Muse Spark 1.2 Contributor")
    },
    DescriptorModel {
        thinking: THINKING_LOW_TO_MAX,
        ..go_anthropic("claude-haiku-5-5", "Claude Haiku 5.5")
    },
    DescriptorModel {
        thinking: THINKING_LOW_TO_MAX,
        ..go_responses("gpt-6-luna", "GPT-6 Luna")
    },
    DescriptorModel {
        thinking: THINKING_HIGH_MAX,
        ..go_openai("deepseek-v4-pro", "DeepSeek V4 Pro (New)")
    },
    DescriptorModel {
        thinking: THINKING_HIGH_ONLY,
        ..go_openai("hy4-preview", "Hy4 preview")
    },
    DescriptorModel {
        thinking: THINKING_MINIMAL_TO_XHIGH,
        ..go_responses("muse-spark-1.3-contributor", "Muse Spark 1.3 Contributor")
    },
    DescriptorModel {
        thinking: THINKING_LOW_MED_HIGH,
        ..go_openai("step-5-preview-free", "Step 5 Preview Free")
    },
    DescriptorModel {
        thinking: THINKING_LOW_HIGH_MAX,
        ..go_openai("glm-5.3", "GLM-5.3")
    },
    go_openai("kimi-k2.7-code", "Kimi K2.7 Code"),
    go_anthropic("qwen3.7-plus", "Qwen3.7 Plus"),
    // Added when the list was resynced to the 2026-10-17 snapshot; upstream cdd8a70 has no
    // `thinkingLevelMap` for either, so neither carries one.
    go_openai("beam-free", "Beam Free"),
    go_openai("step-5-preview", "Step 5 Preview"),
];

/// The models.dev cache's model names, kept as a small built-in fallback (the full list is
/// merged from the cache and `/v1/models`, ADR 0026 D0).
const ANTHROPIC_MODELS: &[DescriptorModel] = &[
    anthropic_model("claude-opus-5-5", "Claude Opus 5.5"),
    anthropic_model("claude-sonnet-4-5", "Claude Sonnet 4.5 (latest)"),
    anthropic_model("claude-haiku-4-5", "Claude Haiku 4.5 (latest)"),
    anthropic_model("claude-opus-4-5", "Claude Opus 4.5 (latest)"),
];

/// The `openai` descriptor's built-in list. models.dev's `openai` provider is `@ai-sdk/openai`
/// (AI SDK 5's default model is the Responses API), so since ADR 0026 every model
/// here is [`Protocol::Responses`]; the 2026-10-17 snapshot carries all four. The table is the
/// fallback the catalog cannot be for, so it agrees with the catalog and the startup line does
/// not report a disagreement.
const OPENAI_MODELS: &[DescriptorModel] = &[
    openai_responses_model("gpt-5.4", "GPT-5.4"),
    openai_responses_model("gpt-5.4-pro", "GPT-5.4 Pro"),
    openai_responses_model("gpt-5.5-pro", "GPT-5.5 Pro"),
    openai_responses_model("gpt-3.5-turbo", "GPT-3.5-turbo"),
];

const fn model(id: &'static str, name: &'static str, protocol: Protocol) -> DescriptorModel {
    DescriptorModel {
        id,
        name,
        protocol,
        // The numbers of every non-`chatgpt` descriptor come from models.dev (ADR 0018).
        limit: ModelLimit::UNKNOWN,
        thinking: &[],
    }
}

/// The anthropic descriptor's provider-level map (ADR 0026 D11): every level gets a distinct
/// wire value, so `/thinking low` and `/thinking max` are not the same request.
///
/// `off` sends no `thinking` key. `medium` — the default — uses `adaptive`, the provider's own
/// budget choice, which is what the adapter already sent. The rest are `enabled:<budget>` with
/// budgets that **fit the default `--max-tokens` (16384)**: the adapter refuses a budget that
/// is not `>= 1024` and `< max_tokens`, so a smaller `--max-tokens` makes the higher levels a
/// startup error (an existing rule, not this map's). These numbers are a proposal — the ADR
/// names `enabled:<budget>` but gives no per-level budgets — and are marked as provisional.
pub const ANTHROPIC_THINKING: ThinkingMap = &[
    ("off", Some("")),
    ("minimal", Some("enabled:1024")),
    ("low", Some("enabled:2048")),
    ("medium", Some("adaptive")),
    ("high", Some("enabled:4096")),
    ("xhigh", Some("enabled:8192")),
    ("max", Some("enabled:12288")),
];

const fn go_openai(id: &'static str, name: &'static str) -> DescriptorModel {
    model(id, name, Protocol::OpenAiCompatible)
}

const fn go_anthropic(id: &'static str, name: &'static str) -> DescriptorModel {
    model(id, name, Protocol::Anthropic)
}

/// A model models.dev lists as `@ai-sdk/openai` on `opencode-go`: the Responses API on the same
/// base URL as the chat models (ADR 0026).
const fn go_responses(id: &'static str, name: &'static str) -> DescriptorModel {
    model(id, name, Protocol::Responses)
}

const fn anthropic_model(id: &'static str, name: &'static str) -> DescriptorModel {
    model(id, name, Protocol::Anthropic)
}

/// An `openai` model: models.dev's `openai` provider is `@ai-sdk/openai`, which AI SDK 5 sends
/// as the Responses API (ADR 0026).
const fn openai_responses_model(id: &'static str, name: &'static str) -> DescriptorModel {
    model(id, name, Protocol::Responses)
}

/// Everything before `/responses` (the adapter's own convention, `OpenAiResponsesConfig::base_url`'s
/// doc comment gives this exact example): the real request goes to
/// `{CHATGPT_BASE}/responses` = `https://chatgpt.com/backend-api/codex/responses`.
const CHATGPT_BASE: &str = "https://chatgpt.com/backend-api/codex";

/// The hard-coded base URL of a built-in `descriptor` for `protocol` (ADR 0026
/// decision 2). `None` when that descriptor has no endpoint for that protocol. The catalog's
/// own `api` field is deliberately **not** consulted: a one-line data change must not be able
/// to redirect a request that carries the user's key.
pub fn descriptor_base_url(descriptor_id: &str, protocol: Protocol) -> Option<&'static str> {
    match (descriptor_id, protocol) {
        ("opencode-go", Protocol::OpenAiCompatible | Protocol::Responses) => {
            Some(OPENCODE_GO_OPENAI_BASE)
        }
        ("opencode-go", Protocol::Anthropic) => Some(OPENCODE_GO_ANTHROPIC_BASE),
        ("anthropic", Protocol::Anthropic) => Some(ANTHROPIC_BASE),
        ("openai", Protocol::OpenAiCompatible | Protocol::Responses) => Some(OPENAI_BASE),
        ("openai-compatible", Protocol::OpenAiCompatible) => Some(OPENAI_BASE),
        ("chatgpt", Protocol::Responses) => Some(CHATGPT_BASE),
        _ => None,
    }
}

const fn chatgpt_model(
    id: &'static str,
    name: &'static str,
    thinking: ThinkingMap,
    limit: ModelLimit,
) -> DescriptorModel {
    DescriptorModel {
        id,
        name,
        protocol: Protocol::Responses,
        limit,
        thinking,
    }
}

/// One `contextWindow`/`maxTokens` pair of the upstream `openai-codex` entry as the descriptor's
/// [`ModelLimit`]: the context window `C` and the output ceiling. The catalog's third number,
/// `limit.input`, has no counterpart there, so it stays unknown (never invented).
const fn codex_limit(context_window: u64, max_tokens: u64) -> ModelLimit {
    ModelLimit {
        context: Some(context_window),
        output: Some(max_tokens),
        input: None,
    }
}

/// `chatgpt`'s built-in model list, from `@oh-my-pi/pi-catalog/src/models.json`'s
/// `openai-codex` block (read-only reference, 2026-10-17: all 8 entries, every one
/// `"api": "openai-codex-responses"`). Two `thinking.efforts` shapes appear upstream:
/// `low/medium/high/xhigh` (→ the existing [`THINKING_UP_TO_XHIGH`]) and
/// `low/medium/high/xhigh/max` (→ the existing [`THINKING_LOW_TO_MAX`]) — no new map constant
/// is needed. **Unverified**: these are upstream's own names, not confirmed against a live
/// ChatGPT subscription; the list is a starting point the ADR says to keep easy to edit.
///
/// The window numbers are the same block's `contextWindow`/`maxTokens`, at the line the entry
/// starts on:
///
/// | model | entry | `contextWindow` | `maxTokens` |
/// |---|---|---|---|
/// | `gpt-5.3-codex-spark` | `:76381-76403` | 128000 | 128000 |
/// | `gpt-5.4` | `:76418-76441` | 272000 | 128000 |
/// | `gpt-5.4-mini` | `:76455-76478` | 272000 | 128000 |
/// | `gpt-5.5` | `:76492-76515` | 272000 | 128000 |
/// | `gpt-5.6-luna` | `:76530-76553` | 1000000 | 128000 |
/// | `gpt-5.6-sol` | `:76569-76592` | 1000000 | 128000 |
/// | `gpt-5.6-terra` | `:76608-76631` | 1000000 | 128000 |
/// | `gpt-daybreak-blue-latest` | `:76647-76670` | 272000 | 128000 |
///
/// models.dev has no `chatgpt` provider, so these numbers are the only window a `chatgpt` route
/// has: without them the session runs with the budget off (ADR 0015/0018).
const CODEX_MODELS: &[DescriptorModel] = &[
    chatgpt_model(
        "gpt-5.3-codex-spark",
        "GPT-5.3 Codex Spark",
        THINKING_UP_TO_XHIGH,
        codex_limit(128_000, 128_000),
    ),
    chatgpt_model(
        "gpt-5.4",
        "GPT-5.4",
        THINKING_UP_TO_XHIGH,
        codex_limit(272_000, 128_000),
    ),
    chatgpt_model(
        "gpt-5.4-mini",
        "GPT-5.4 mini",
        THINKING_UP_TO_XHIGH,
        codex_limit(272_000, 128_000),
    ),
    chatgpt_model(
        "gpt-5.5",
        "GPT-5.5",
        THINKING_UP_TO_XHIGH,
        codex_limit(272_000, 128_000),
    ),
    chatgpt_model(
        "gpt-5.6-luna",
        "GPT-5.6 Luna",
        THINKING_LOW_TO_MAX,
        codex_limit(1_000_000, 128_000),
    ),
    chatgpt_model(
        "gpt-5.6-sol",
        "GPT-5.6 Sol",
        THINKING_LOW_TO_MAX,
        codex_limit(1_000_000, 128_000),
    ),
    chatgpt_model(
        "gpt-5.6-terra",
        "GPT-5.6 Terra",
        THINKING_LOW_TO_MAX,
        codex_limit(1_000_000, 128_000),
    ),
    chatgpt_model(
        "gpt-daybreak-blue-latest",
        "Daybreak Blue",
        THINKING_LOW_TO_MAX,
        codex_limit(272_000, 128_000),
    ),
];

/// The built-in descriptors (ADR 0026 D0), in a stable order.
pub const DESCRIPTORS: &[ProviderDescriptor] = &[
    ProviderDescriptor {
        id: "opencode-go",
        display_name: "OpenCode Go",
        default_base_url: OPENCODE_GO_OPENAI_BASE,
        login: LoginMethod::ApiKey { extra_fields: &[] },
        thinking: &[],
        default_protocol: Protocol::OpenAiCompatible,
        // D3: the chat base serves no `/models` , so `/login`
        // verifies against the usage endpoint with the footer's own Bearer auth.
        verification_url: Some(crate::opencode_go::USAGE_URL),
        models_url: Some(OPENCODE_GO_MODELS_URL),
        models: OPENCODE_GO_MODELS,
        headers: opencode_go_headers,
        // ADR 0027: the endpoint source `opencode_go.rs` used to be, now
        // declared here. The key comes from `auth.json` (ADR 0026 D2) and never leaves the
        // process.
        usage: Some(UsageSource::Endpoint(EndpointUsage {
            url: crate::opencode_go::USAGE_URL,
            legacy_item_ids: &["opencode-go.usage"],
            build: crate::opencode_go::endpoint,
        })),
    },
    ProviderDescriptor {
        id: "chatgpt",
        display_name: "ChatGPT (subscription)",
        default_base_url: CHATGPT_BASE,
        // Browser OAuth is the primary flow; the device-code flow is a second `/login` entry
        // (ADR 0026). Neither is dispatched through this field today —
        // both are special-cased by id in the TUI wizard and `radian login` (slice ①) — but it
        // documents which login this descriptor actually uses.
        login: LoginMethod::OAuthBrowser,
        thinking: &[],
        default_protocol: Protocol::Responses,
        verification_url: None,
        models_url: None,
        models: CODEX_MODELS,
        headers: chatgpt_headers,
        // ADR 0026 + ADR 0027: `/wham/usage`,
        // refreshed with the login the fetch renews itself per call.
        usage: Some(UsageSource::Endpoint(EndpointUsage {
            url: crate::chatgpt_usage::USAGE_URL,
            legacy_item_ids: &["chatgpt.usage"],
            build: crate::chatgpt_usage::endpoint,
        })),
    },
    ProviderDescriptor {
        id: "anthropic",
        display_name: "Anthropic",
        default_base_url: "https://api.anthropic.com",
        login: LoginMethod::ApiKey { extra_fields: &[] },
        thinking: ANTHROPIC_THINKING,
        default_protocol: Protocol::Anthropic,
        verification_url: None,
        models_url: None,
        models: ANTHROPIC_MODELS,
        headers: no_headers,
        // ADR 0027 slice ④: the `anthropic-ratelimit-*` response
        // headers, parsed per response (no endpoint, no key).
        usage: Some(UsageSource::Headers(
            crate::headers_usage::ANTHROPIC_HEADERS,
        )),
    },
    ProviderDescriptor {
        id: "openai",
        display_name: "OpenAI",
        default_base_url: "https://api.openai.com/v1",
        login: LoginMethod::ApiKey { extra_fields: &[] },
        thinking: &[],
        default_protocol: Protocol::OpenAiCompatible,
        verification_url: None,
        models_url: None,
        models: OPENAI_MODELS,
        headers: no_headers,
        // ADR 0027 slice ④: the `x-ratelimit-*` response headers,
        // parsed per response (no endpoint, no key).
        usage: Some(UsageSource::Headers(crate::headers_usage::OPENAI_HEADERS)),
    },
    ProviderDescriptor {
        id: "openai-compatible",
        display_name: "OpenAI-compatible",
        default_base_url: "https://api.openai.com/v1",
        login: LoginMethod::ApiKey {
            // The base URL and the custom id are supplied at `/login` and stored in the
            // entry's `env` (ADR 0026 D1/D3).
            extra_fields: &[crate::auth::RADIAN_BASE_URL, "id"],
        },
        thinking: &[],
        default_protocol: Protocol::OpenAiCompatible,
        verification_url: None,
        models_url: None,
        // The user supplies the model id; there is no built-in list.
        models: &[],
        headers: no_headers,
        usage: None,
    },
];

/// The descriptor for `id`, if it is built in.
pub fn descriptor(id: &str) -> Option<&'static ProviderDescriptor> {
    DESCRIPTORS.iter().find(|entry| entry.id == id)
}

/// The built-in descriptors.
pub fn descriptors() -> &'static [ProviderDescriptor] {
    DESCRIPTORS
}

/// The generic descriptor's id, the only one `--base-url`/`--header` apply to (ADR 0026 D0).
pub const GENERIC_ID: &str = "openai-compatible";

/// A resolved route: which adapter, base URL and headers to use for one model (ADR 0026 D0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub descriptor_id: &'static str,
    pub model: String,
    pub protocol: Protocol,
    pub base_url: String,
    /// The headers for every request, `{session_id}` already expanded.
    pub headers: Vec<(String, String)>,
    /// A one-line startup notice when the **catalog** decided this route's wire protocol and it
    /// differs from the built-in table's ([`ProviderRef::resolve_protocol`]): `"protocol from
    /// models.dev"` (ADR 0026). `None` when the table stands.
    pub protocol_notice: Option<String>,
}

impl Route {
    pub fn adapter(&self) -> Provider {
        self.protocol.adapter()
    }
}

/// Resolves a descriptor id, a model, the generic base URL/headers and the credential entry's
/// base URL into a [`Route`] (ADR 0026 D0). The [`ProviderRef`] form is [`resolve_route_for`].
///
/// `base_url_override` is `--base-url` or config `model.base_url`, `auth_base_url` is the
/// resolved `RADIAN_BASE_URL` from the `auth.json` entry's `env`, and `header_args` are the
/// `--header`/config headers. The last two are refused for a non-generic descriptor, because
/// only the generic provider lets the user choose the endpoint.
pub fn resolve_route(
    descriptor_id: &str,
    model: &str,
    base_url_override: Option<&str>,
    auth_base_url: Option<&str>,
    header_args: &[(String, String)],
    session_id: &str,
    catalog: Option<&crate::Catalog>,
) -> Result<Route, String> {
    let descriptor = descriptor(descriptor_id).ok_or_else(|| {
        format!(
            "unknown provider {descriptor_id:?}: choose one of {}",
            descriptors()
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    resolve_route_for(
        &ProviderRef::BuiltIn(descriptor),
        model,
        base_url_override,
        auth_base_url,
        header_args,
        session_id,
        catalog,
    )
}

/// [`resolve_route`] for a [`ProviderRef`]: a named provider's endpoint and protocol come
/// from its declaration, so `--base-url`/`--header` (and config `model.base_url`/`headers`)
/// are refused for it too — they are the generic provider's dials (ADR 0026 D10).
///
/// `catalog` is where a built-in descriptor's wire protocol comes from when it carries the
/// model (ADR 0026); `None` uses the built-in table.
pub fn resolve_route_for(
    provider: &ProviderRef,
    model: &str,
    base_url_override: Option<&str>,
    auth_base_url: Option<&str>,
    header_args: &[(String, String)],
    session_id: &str,
    catalog: Option<&crate::Catalog>,
) -> Result<Route, String> {
    let generic = provider.takes_base_url_override();
    if !generic && base_url_override.is_some() {
        return Err(format!(
            "--base-url (or model.base_url) only applies to --provider {GENERIC_ID}; {} uses \
             its own endpoint",
            provider.id()
        ));
    }
    if !generic && !header_args.is_empty() {
        return Err(format!(
            "--header (or model.headers) only applies to --provider {GENERIC_ID}; {} sets its \
             own request headers",
            provider.id()
        ));
    }
    let protocol = provider.resolve_protocol(catalog, model)?;
    // ADR 0026: when the catalog's `npm` overrules the built-in table,
    // the startup line says so once. A model the catalog does not carry keeps the table's
    // protocol, so there is nothing to report.
    let protocol_notice = (matches!(provider, ProviderRef::BuiltIn(_))
        && !generic
        && catalog.is_some_and(|catalog| catalog.npm(provider.id(), model).is_some())
        && protocol != provider.protocol_for(model))
    .then(|| "protocol from models.dev".to_owned());
    let base_url = if generic {
        // `--base-url`/config wins, then the credential entry's base URL, then the descriptor.
        base_url_override
            .map(str::to_owned)
            .or_else(|| auth_base_url.map(str::to_owned))
            .unwrap_or_else(|| {
                descriptor_base_url(GENERIC_ID, Protocol::OpenAiCompatible)
                    .expect("the generic descriptor has a base URL")
                    .to_owned()
            })
    } else {
        provider.endpoint_for(protocol, auth_base_url)?
    };
    let mut headers = provider.request_headers(session_id);
    if generic {
        // The descriptor has no headers of its own, so the user's are all of them.
        headers.extend(header_args.iter().cloned());
    }
    Ok(Route {
        descriptor_id: match provider {
            ProviderRef::BuiltIn(descriptor) => descriptor.id,
            ProviderRef::Named(_) => GENERIC_ID,
        },
        model: model.to_owned(),
        protocol,
        base_url,
        headers,
        protocol_notice,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR 0027: only the providers whose usage the footer can fetch
    /// declare a source; the endpoint providers poll, the headers providers (slice ④) read
    /// each response. Every other descriptor declares none, so its `usage` row can never
    /// appear.
    #[test]
    fn only_the_endpoint_providers_declare_a_usage_source() {
        let declared: Vec<(&str, &str)> = descriptors()
            .iter()
            .filter_map(|entry| match &entry.usage {
                Some(UsageSource::Endpoint(endpoint)) => Some((entry.id, endpoint.url)),
                _ => None,
            })
            .collect();
        assert_eq!(
            declared,
            vec![
                ("opencode-go", "https://opencode.ai/zen/go/v1/usage"),
                ("chatgpt", "https://chatgpt.com/backend-api/wham/usage"),
            ]
        );
        // Slice ④: `anthropic` and `openai` read their rate-limit headers per response.
        for id in ["anthropic", "openai"] {
            assert!(has_usage_source(id), "{id} declares a usage source");
            assert!(usage_endpoint(id).is_none(), "{id} has no endpoint to poll");
            assert!(
                matches!(descriptor(id).unwrap().usage, Some(UsageSource::Headers(_))),
                "{id} declares the Headers source"
            );
        }
        assert!(usage_endpoint("openai-compatible").is_none());
        assert!(!has_usage_source("openai-compatible"));
        assert!(has_usage_source("opencode-go") && has_usage_source("chatgpt"));
        assert!(usage_endpoint("no-such-provider").is_none());
        assert!(!has_usage_source("no-such-provider"));
    }

    /// ADR 0027: the ids an earlier version used resolve to `usage` for
    /// one version; every other id is left as written.
    #[test]
    fn the_earlier_usage_ids_are_aliases_of_usage() {
        assert_eq!(usage_item_alias("opencode-go.usage"), Some("usage"));
        assert_eq!(usage_item_alias("chatgpt.usage"), Some("usage"));
        assert_eq!(usage_item_alias("usage"), None);
        assert_eq!(usage_item_alias("model"), None);
    }

    /// The fetch a descriptor builds names the descriptor's own id, and is built from the
    /// descriptor's own URL: the `usage` component draws a snapshot only while it belongs to
    /// the current route's provider.
    #[test]
    fn a_built_fetch_names_its_own_provider() {
        for id in ["opencode-go", "chatgpt"] {
            let endpoint = usage_endpoint(id).expect("a declared source");
            let fetcher = (endpoint.build)(&UsageArgs {
                url: endpoint.url.to_owned(),
                session_id: "sess-1".to_owned(),
                secret: Some("sk-not-used-by-this-test".to_owned()),
                pool: None,
            });
            assert_eq!(fetcher.id(), id);
        }
    }

    #[test]
    fn the_built_ins_are_registered() {
        let ids: Vec<&str> = descriptors().iter().map(|d| d.id).collect();
        assert_eq!(
            ids,
            [
                "opencode-go",
                "chatgpt",
                "anthropic",
                "openai",
                "openai-compatible"
            ]
        );
        assert!(descriptor("nope").is_none());
        assert_eq!(
            descriptor("anthropic").unwrap().default_base_url,
            "https://api.anthropic.com"
        );
        // models.dev's `openai` provider is `@ai-sdk/openai`, so its models are Responses
        // (ADR 0026), on the OpenAI base.
        for model in descriptor("openai").unwrap().models {
            assert_eq!(model.protocol, Protocol::Responses, "{}", model.id);
        }
        assert_eq!(
            descriptor_base_url("openai", Protocol::Responses),
            Some("https://api.openai.com/v1")
        );
        assert_eq!(
            descriptor_base_url("anthropic", Protocol::Responses),
            None,
            "the anthropic descriptor has no Responses endpoint"
        );
    }

    /// ADR 0026 slice 3: the descriptor exists, uses the Responses protocol, every
    /// model resolves to the codex base URL, and its static headers carry `originator: radian`
    /// and the session id but **not** `Authorization`/`chatgpt-account-id` (those are per-call,
    /// `ChatGptModel`'s job).
    #[test]
    fn the_chatgpt_descriptor_is_registered_with_responses_models_and_static_headers_only() {
        let chatgpt = descriptor("chatgpt").expect("the chatgpt descriptor");
        assert_eq!(chatgpt.default_protocol, Protocol::Responses);
        assert_eq!(chatgpt.default_base_url, CHATGPT_BASE);
        assert!(!chatgpt.models.is_empty(), "the built-in model list");
        for model in chatgpt.models {
            assert_eq!(model.protocol, Protocol::Responses, "{}", model.id);
        }
        assert_eq!(
            descriptor_base_url("chatgpt", Protocol::Responses),
            Some(CHATGPT_BASE)
        );
        assert_eq!(chatgpt.protocol_for("gpt-5.4"), Protocol::Responses);
        assert_eq!(Protocol::Responses.adapter(), Provider::ChatGpt);
        assert_eq!(Protocol::Responses.adapter_name(), "openai-responses");

        let headers = chatgpt.request_headers("sess-1");
        assert_eq!(
            headers,
            vec![
                (
                    "OpenAI-Beta".to_owned(),
                    "responses=experimental".to_owned()
                ),
                ("originator".to_owned(), "radian".to_owned()),
                ("session_id".to_owned(), "sess-1".to_owned()),
            ]
        );
        assert!(
            !headers.iter().any(|(name, _)| name == "Authorization"
                || name.eq_ignore_ascii_case("chatgpt-account-id")),
            "the dynamic headers must not be here: {headers:?}"
        );
    }

    /// `chatgpt` is not doubly listed: `login_choices()` excludes it from the generic
    /// descriptor loop because it already has its own two entries (ADR 0026
    /// decision 3, slice ① `oauth_chatgpt::PROVIDER_ID` + `CHATGPT_DEVICE_LOGIN`).
    #[test]
    fn login_choices_lists_chatgpt_exactly_once_each_for_browser_and_device() {
        let choices = login_choices();
        let chatgpt_entries: Vec<&str> = choices
            .iter()
            .map(|(id, _)| id.as_str())
            .filter(|id| id.starts_with("chatgpt"))
            .collect();
        assert_eq!(
            chatgpt_entries,
            [crate::oauth_chatgpt::PROVIDER_ID, CHATGPT_DEVICE_LOGIN]
        );
    }

    #[test]
    fn opencode_go_headers_match_the_today_header_plus_the_client() {
        let session_id = "abc-123";
        let today = crate::headers::parse_headers(
            &[format!("x-opencode-session={session_id}")],
            session_id,
        )
        .unwrap();
        let headers = descriptor("opencode-go")
            .unwrap()
            .request_headers(session_id);
        assert_eq!(
            headers[0], today[0],
            "the session header must be byte-identical"
        );
        assert_eq!(
            headers[1],
            ("x-opencode-client".to_owned(), "radian".to_owned())
        );
        // The other descriptors add nothing.
        assert!(
            descriptor("anthropic")
                .unwrap()
                .request_headers(session_id)
                .is_empty()
        );
    }

    #[test]
    fn opencode_go_maps_models_to_the_three_protocols() {
        let go = descriptor("opencode-go").unwrap();
        assert_eq!(
            go.protocol_for("deepseek-v4.1-flash"),
            Protocol::OpenAiCompatible
        );
        assert_eq!(go.protocol_for("claude-haiku-5-5"), Protocol::Anthropic);
        assert_eq!(go.protocol_for("qwen3.7-plus"), Protocol::Anthropic);
        assert_eq!(go.protocol_for("mimo-v2.6-pro"), Protocol::OpenAiCompatible);
        // ADR 0026: the five `@ai-sdk/openai` models are Responses in the table too.
        for model in [
            "grok-4.7",
            "gpt-5.6-luna",
            "gpt-6-luna",
            "muse-spark-1.2-contributor",
            "muse-spark-1.3-contributor",
        ] {
            assert_eq!(go.protocol_for(model), Protocol::Responses, "{model}");
        }
        // The two models the 2026-10-17 snapshot added; the two it dropped are gone.
        assert!(go.model("beam-free").is_some());
        assert!(go.model("step-5-preview").is_some());
        assert!(go.model("deepseek-v4-flash").is_none());
        assert!(go.model("grok-4.6").is_none());
        assert!(
            go.model("gemini-4-argon-contributor").is_none(),
            "the Google model is refused through the catalog, never listed here"
        );
        // A model the list does not name uses the descriptor default (openai-compatible).
        assert_eq!(go.protocol_for("not-a-model"), Protocol::OpenAiCompatible);
        // The base URL is per (descriptor, protocol): chat and Responses share the OpenAI base,
        // Anthropic has its own, and neither is models.dev's `api` verbatim.
        assert_eq!(
            descriptor_base_url("opencode-go", Protocol::OpenAiCompatible),
            Some("https://opencode.ai/inference/go/openai/v1")
        );
        assert_eq!(
            descriptor_base_url("opencode-go", Protocol::Responses),
            Some("https://opencode.ai/inference/go/openai/v1")
        );
        assert_eq!(
            descriptor_base_url("opencode-go", Protocol::Anthropic),
            Some("https://opencode.ai/inference/go/anthropic"),
            "the Anthropic adapter appends `/v1/messages`, so the base stops before `/v1`"
        );
        // The Anthropic protocol maps to the Anthropic adapter.
        assert_eq!(Protocol::Anthropic.adapter_name(), "anthropic");
        assert_eq!(Protocol::Anthropic.adapter(), Provider::Anthropic);
    }

    /// ADR 0026: with no cache, the bundled snapshot alone decides the
    /// protocol — chat, Anthropic and Responses — and refuses the one it has no adapter for.
    #[test]
    fn the_bundled_snapshot_decides_the_protocol_with_no_cache() {
        let snapshot = crate::bundled_catalog();
        let builtin = ProviderRef::BuiltIn(descriptor("opencode-go").unwrap());
        assert_eq!(
            builtin
                .resolve_protocol(Some(snapshot), "deepseek-v4.1-flash")
                .unwrap(),
            Protocol::OpenAiCompatible
        );
        assert_eq!(
            builtin
                .resolve_protocol(Some(snapshot), "claude-haiku-5-5")
                .unwrap(),
            Protocol::Anthropic
        );
        assert_eq!(
            builtin
                .resolve_protocol(Some(snapshot), "muse-spark-1.3-contributor")
                .unwrap(),
            Protocol::Responses
        );
        assert_eq!(
            builtin
                .resolve_protocol(Some(snapshot), "gemini-4-argon-contributor")
                .unwrap_err(),
            "radian does not support this model's API (@ai-sdk/google)"
        );
        // The snapshot's endpoint is hard-coded per protocol, not read from it.
        let route = resolve_route(
            "opencode-go",
            "muse-spark-1.3-contributor",
            None,
            None,
            &[],
            "s",
            Some(snapshot),
        )
        .unwrap();
        assert_eq!(route.protocol, Protocol::Responses);
        assert_eq!(route.base_url, "https://opencode.ai/inference/go/openai/v1");
    }

    /// The shipped table and the bundled snapshot agree, so no startup prints `protocol from
    /// models.dev` (ADR 0026). The notice is for genuine upstream drift,
    /// not for a table that was not resynced to the snapshot it ships with.
    #[test]
    fn the_built_in_table_agrees_with_the_bundled_snapshot() {
        let snapshot = crate::bundled_catalog();
        for descriptor in descriptors() {
            for model in descriptor.models {
                let route = resolve_route(
                    descriptor.id,
                    model.id,
                    None,
                    None,
                    &[],
                    "s",
                    Some(snapshot),
                )
                .unwrap_or_else(|error| panic!("{}/{}: {error}", descriptor.id, model.id));
                assert_eq!(
                    route.protocol_notice, None,
                    "{}/{}: the table and the snapshot disagree",
                    descriptor.id, model.id
                );
            }
        }
    }

    /// A catalog with only the `opencode-go` facts the protocol tests need.
    fn catalog(text: &str) -> crate::Catalog {
        crate::Catalog::parse(text).expect("the test catalog parses")
    }

    /// ADR 0026: a built-in descriptor's model takes its protocol from
    /// the catalog's `npm`, and an `npm` with no adapter is refused by name.
    #[test]
    fn the_catalog_npm_decides_the_protocol_and_an_unknown_one_is_refused() {
        let catalog = catalog(
            r#"{
                "opencode-go": {
                    "npm": "@ai-sdk/openai-compatible",
                    "models": {
                        "chat-model": {"limit": {"context": 5}},
                        "anth-model": {"limit": {"context": 5},
                            "provider": {"npm": "@ai-sdk/anthropic"}},
                        "resp-model": {"limit": {"context": 5},
                            "provider": {"npm": "@ai-sdk/openai"}},
                        "google-model": {"limit": {"context": 5},
                            "provider": {"npm": "@ai-sdk/google"}}
                    }
                }
            }"#,
        );
        let go = descriptor("opencode-go").unwrap();
        let builtin = ProviderRef::BuiltIn(go);
        // The catalog's npm wins over the table for every one of these.
        assert_eq!(
            builtin
                .resolve_protocol(Some(&catalog), "chat-model")
                .unwrap(),
            Protocol::OpenAiCompatible
        );
        assert_eq!(
            builtin
                .resolve_protocol(Some(&catalog), "anth-model")
                .unwrap(),
            Protocol::Anthropic
        );
        assert_eq!(
            builtin
                .resolve_protocol(Some(&catalog), "resp-model")
                .unwrap(),
            Protocol::Responses
        );
        // `@ai-sdk/google` has no adapter: refused, never silently sent as chat.
        let error = builtin
            .resolve_protocol(Some(&catalog), "google-model")
            .unwrap_err();
        assert_eq!(
            error,
            "radian does not support this model's API (@ai-sdk/google)"
        );
        // A model the catalog does not carry falls back to the built-in table.
        assert_eq!(
            builtin
                .resolve_protocol(Some(&catalog), "muse-spark-1.3-contributor")
                .unwrap(),
            Protocol::Responses,
            "the table's own protocol"
        );
        assert_eq!(
            builtin.resolve_protocol(None, "claude-haiku-5-5").unwrap(),
            Protocol::Anthropic
        );
    }

    /// ADR 0026: the catalog's `api` field is never the endpoint. The
    /// route goes to the hard-coded (descriptor, protocol) base even when the catalog names a
    /// different host for the model.
    #[test]
    fn the_catalogs_api_field_is_never_used_for_the_endpoint() {
        let catalog = catalog(
            r#"{
                "opencode-go": {
                    "npm": "@ai-sdk/openai-compatible",
                    "api": "https://evil.example/v1",
                    "models": {
                        "resp-model": {"limit": {"context": 5},
                            "provider": {"npm": "@ai-sdk/openai",
                                         "api": "https://evil.example/responses"}},
                        "chat-model": {"limit": {"context": 5},
                            "api": "https://evil.example/chat"}
                    }
                }
            }"#,
        );
        let responses = resolve_route(
            "opencode-go",
            "resp-model",
            None,
            None,
            &[],
            "s",
            Some(&catalog),
        )
        .unwrap();
        assert_eq!(responses.protocol, Protocol::Responses);
        assert_eq!(
            responses.base_url, "https://opencode.ai/inference/go/openai/v1",
            "the catalog's api must not be used"
        );
        let chat = resolve_route(
            "opencode-go",
            "chat-model",
            None,
            None,
            &[],
            "s",
            Some(&catalog),
        )
        .unwrap();
        assert_eq!(chat.base_url, "https://opencode.ai/inference/go/openai/v1");
        // The notice fires only when the catalog overrules the table (here it does not).
        assert_eq!(chat.protocol_notice, None);
    }

    /// ADR 0026: when the catalog's `npm` disagrees with the built-in
    /// table, the catalog wins and the route carries the one-line notice.
    #[test]
    fn the_catalog_overruling_the_table_is_noticed() {
        let catalog = catalog(
            r#"{
                "opencode-go": {
                    "npm": "@ai-sdk/openai-compatible",
                    "models": {
                        "muse-spark-1.3-contributor": {"limit": {"context": 5},
                            "provider": {"npm": "@ai-sdk/openai"}},
                        "mimo-v2.6-pro": {"limit": {"context": 5},
                            "provider": {"npm": "@ai-sdk/openai"}}
                    }
                }
            }"#,
        );
        // The table says chat for mimo-v2.6-pro; the catalog says Responses.
        let route = resolve_route(
            "opencode-go",
            "mimo-v2.6-pro",
            None,
            None,
            &[],
            "s",
            Some(&catalog),
        )
        .unwrap();
        assert_eq!(route.protocol, Protocol::Responses);
        assert_eq!(
            route.protocol_notice.as_deref(),
            Some("protocol from models.dev")
        );
        // Both agree on muse-spark-1.3-contributor (Responses), so there is nothing to say.
        let agree = resolve_route(
            "opencode-go",
            "muse-spark-1.3-contributor",
            None,
            None,
            &[],
            "s",
            Some(&catalog),
        )
        .unwrap();
        assert_eq!(agree.protocol, Protocol::Responses);
        assert_eq!(agree.protocol_notice, None);
    }

    #[test]
    fn a_route_picks_the_adapter_base_url_and_headers() {
        // opencode-go: an openai-compatible model on the inference base, with the session and
        // client headers.
        let route = resolve_route(
            "opencode-go",
            "deepseek-v4.1-flash",
            None,
            None,
            &[],
            "s1",
            None,
        )
        .unwrap();
        assert_eq!(route.protocol, Protocol::OpenAiCompatible);
        assert_eq!(route.base_url, "https://opencode.ai/inference/go/openai/v1");
        assert_eq!(
            route.headers,
            vec![
                ("x-opencode-session".to_owned(), "s1".to_owned()),
                ("x-opencode-client".to_owned(), "radian".to_owned())
            ]
        );
        // Its Anthropic model goes to the Anthropic base with the Anthropic adapter.
        let anthropic = resolve_route(
            "opencode-go",
            "claude-haiku-5-5",
            None,
            None,
            &[],
            "s1",
            None,
        )
        .unwrap();
        assert_eq!(anthropic.protocol, Protocol::Anthropic);
        assert_eq!(
            anthropic.base_url,
            "https://opencode.ai/inference/go/anthropic"
        );
        // Its Responses model shares the OpenAI base (the adapter appends `/responses`).
        let responses = resolve_route(
            "opencode-go",
            "muse-spark-1.3-contributor",
            None,
            None,
            &[],
            "s1",
            None,
        )
        .unwrap();
        assert_eq!(responses.protocol, Protocol::Responses);
        assert_eq!(
            responses.base_url,
            "https://opencode.ai/inference/go/openai/v1"
        );
        // A model neither the catalog nor the list names uses the descriptor default.
        let unknown_model =
            resolve_route("opencode-go", "future-model", None, None, &[], "s1", None).unwrap();
        assert_eq!(unknown_model.protocol, Protocol::OpenAiCompatible);
    }

    #[test]
    fn only_the_generic_descriptor_accepts_a_base_url_or_headers() {
        let headers = vec![("x-custom".to_owned(), "1".to_owned())];
        // A non-generic descriptor refuses both.
        assert!(
            resolve_route("opencode-go", "m", Some("https://x"), None, &[], "s", None)
                .unwrap_err()
                .contains("--base-url")
        );
        assert!(
            resolve_route("anthropic", "m", None, None, &headers, "s", None)
                .unwrap_err()
                .contains("--header")
        );
        // The generic one accepts both, and the user's header is added.
        let route = resolve_route(
            GENERIC_ID,
            "m",
            Some("https://example.com/v1"),
            None,
            &headers,
            "s",
            None,
        )
        .unwrap();
        assert_eq!(route.base_url, "https://example.com/v1");
        assert_eq!(route.protocol, Protocol::OpenAiCompatible);
        assert!(
            route
                .headers
                .contains(&("x-custom".to_owned(), "1".to_owned()))
        );
    }

    #[test]
    fn the_generic_base_url_precedence_is_override_then_auth_then_default() {
        let with_override = resolve_route(
            GENERIC_ID,
            "m",
            Some("https://cli"),
            Some("https://auth"),
            &[],
            "s",
            None,
        )
        .unwrap();
        assert_eq!(with_override.base_url, "https://cli");
        let with_auth =
            resolve_route(GENERIC_ID, "m", None, Some("https://auth"), &[], "s", None).unwrap();
        assert_eq!(with_auth.base_url, "https://auth");
        let default = resolve_route(GENERIC_ID, "m", None, None, &[], "s", None).unwrap();
        assert_eq!(default.base_url, "https://api.openai.com/v1");
        // An unknown descriptor is refused with the list.
        let error = resolve_route("nope", "m", None, None, &[], "s", None).unwrap_err();
        assert!(error.contains("unknown provider"), "{error}");
        assert!(error.contains("opencode-go"), "{error}");
    }

    /// A built-in descriptor still honors a base URL from the credential entry's `env` (the
    /// only override a non-generic route accepts), so an existing user config keeps working.
    #[test]
    fn a_built_in_route_takes_the_auth_entries_base_url_override() {
        let route = resolve_route(
            "opencode-go",
            "deepseek-v4.1-flash",
            None,
            Some("https://mirror.example/v1"),
            &[],
            "s",
            None,
        )
        .unwrap();
        assert_eq!(route.base_url, "https://mirror.example/v1");
    }

    #[test]
    fn the_generic_descriptor_asks_for_the_base_url_and_id() {
        match descriptor("openai-compatible").unwrap().login {
            LoginMethod::ApiKey { extra_fields } => {
                assert_eq!(extra_fields, &[crate::auth::RADIAN_BASE_URL, "id"]);
            }
            other => panic!("expected ApiKey, got {other:?}"),
        }
        assert!(descriptor("openai-compatible").unwrap().models.is_empty());
    }

    // ---------- named providers (ADR 0026 D10) ----------

    fn decl(kind: &str, base_url: &str) -> ProviderDecl {
        ProviderDecl {
            kind: kind.to_owned(),
            base_url: base_url.to_owned(),
            reasoning_replay: None,
        }
    }

    fn env(base_url: &str, kind: Option<&str>) -> BTreeMap<String, String> {
        let mut map = BTreeMap::new();
        map.insert(crate::auth::RADIAN_BASE_URL.to_owned(), base_url.to_owned());
        if let Some(kind) = kind {
            map.insert(RADIAN_KIND.to_owned(), kind.to_owned());
        }
        map
    }

    #[test]
    fn names_are_validated_and_reserved_names_refused() {
        assert!(valid_name("corp"));
        assert!(valid_name("my-host_2"));
        assert!(!valid_name(""));
        assert!(!valid_name("has space"));
        assert!(!valid_name("slash/ed"));
        assert!(!valid_name("../escape"));
        assert!(name_error("corp").is_none());
        assert!(name_error("openai-compatible").is_some());
        assert!(name_error("anthropic").is_some(), "built-ins are reserved");
        assert!(name_error("has space").is_some());
    }

    #[test]
    fn a_named_provider_resolves_from_config_and_the_generic_id_stays_builtin() {
        let mut config = BTreeMap::new();
        config.insert(
            "corp".to_owned(),
            decl("openai-compatible", "https://corp.example/v1"),
        );
        let (provider, warning) = resolve_provider("corp", &config, None).expect("corp resolves");
        assert!(warning.is_none());
        let ProviderRef::Named(named) = &provider else {
            panic!("expected named, got {provider:?}")
        };
        assert_eq!(named.protocol, Protocol::OpenAiCompatible);
        assert_eq!(named.base_url, "https://corp.example/v1");
        // The key is stored under the name, not the descriptor id (D10's whole point).
        assert_eq!(provider.key_id(), "corp");
        assert_eq!(provider.id(), "corp");
        assert_eq!(
            provider.protocol_for("any-model"),
            Protocol::OpenAiCompatible
        );
        assert_eq!(
            provider
                .endpoint_for(Protocol::OpenAiCompatible, None)
                .unwrap(),
            "https://corp.example/v1"
        );
        // The reserved name is the built-in generic, never a lookup.
        let (generic, warning) = resolve_provider(GENERIC_ID, &config, None).unwrap();
        assert!(matches!(generic, ProviderRef::BuiltIn(_)));
        assert_eq!(generic.key_id(), GENERIC_ID);
        assert!(warning.is_none());
    }

    #[test]
    fn anthropic_kind_names_the_anthropic_protocol() {
        let mut config = BTreeMap::new();
        config.insert(
            "relay".to_owned(),
            decl("anthropic", "https://relay.example"),
        );
        let (provider, _) = resolve_provider("relay", &config, None).unwrap();
        assert_eq!(provider.protocol_for("m"), Protocol::Anthropic);
        // An unknown kind is an error, never a guess.
        config.insert("bad".to_owned(), decl("grpc", "https://x"));
        let error = resolve_provider("bad", &config, None).unwrap_err();
        assert!(error.contains("openai-compatible|anthropic"), "{error}");
    }

    #[test]
    fn auth_env_is_the_fallback_and_config_wins_with_a_warning() {
        let auth = env("https://auth.example/v1", Some("anthropic"));
        let (provider, warning) = resolve_provider("solo", &BTreeMap::new(), Some(&auth)).unwrap();
        assert!(warning.is_none());
        assert_eq!(
            provider.endpoint_for(Protocol::Anthropic, None).unwrap(),
            "https://auth.example/v1"
        );
        assert_eq!(provider.protocol_for("m"), Protocol::Anthropic);

        // `RADIAN_KIND` defaults to openai-compatible when absent.
        let (provider, _) = resolve_provider(
            "solo",
            &BTreeMap::new(),
            Some(&env("https://auth.example/v1", None)),
        )
        .unwrap();
        assert_eq!(provider.protocol_for("m"), Protocol::OpenAiCompatible);

        // Both sources: config wins, warn once.
        let mut config = BTreeMap::new();
        config.insert(
            "solo".to_owned(),
            decl("anthropic", "https://config.example"),
        );
        let (provider, warning) = resolve_provider("solo", &config, Some(&auth)).unwrap();
        assert_eq!(
            provider.endpoint_for(Protocol::Anthropic, None).unwrap(),
            "https://config.example"
        );
        let warning = warning.expect("both sources warn");
        assert!(warning.contains("config"), "{warning}");

        // An auth entry without a base URL cannot become a provider.
        let error =
            resolve_provider("ghost", &BTreeMap::new(), Some(&BTreeMap::new())).unwrap_err();
        assert!(error.contains("RADIAN_BASE_URL"), "{error}");
    }

    #[test]
    fn an_unknown_name_errors_with_the_builtins() {
        let error = resolve_provider("nope", &BTreeMap::new(), None).unwrap_err();
        assert!(error.contains("unknown provider"), "{error}");
        assert!(error.contains("opencode-go"), "{error}");
        assert!(error.contains("[providers.nope]"), "{error}");
    }

    #[test]
    fn a_named_route_has_a_fixed_endpoint_and_refuses_the_generic_dials() {
        let mut config = BTreeMap::new();
        config.insert(
            "corp".to_owned(),
            decl("openai-compatible", "https://corp.example/v1"),
        );
        let (provider, _) = resolve_provider("corp", &config, None).unwrap();
        let route = resolve_route_for(&provider, "m1", None, None, &[], "s1", None).unwrap();
        assert_eq!(route.base_url, "https://corp.example/v1");
        assert_eq!(route.protocol, Protocol::OpenAiCompatible);
        assert!(route.headers.is_empty(), "a named provider has no headers");
        // The generic-only dials are refused.
        assert!(
            resolve_route_for(&provider, "m", Some("https://x"), None, &[], "s", None)
                .unwrap_err()
                .contains("--base-url")
        );
        let headers = vec![("x".to_owned(), "1".to_owned())];
        assert!(
            resolve_route_for(&provider, "m", None, None, &headers, "s", None)
                .unwrap_err()
                .contains("--header")
        );
    }
}

// ---------- the unified thinking levels (ADR 0026 D11) ----------

/// The seven unified thinking levels, in **rank order**. The clamp walks this order, so the
/// array order is load-bearing.
pub const THINKING_LEVELS: &[&str] = &["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// The default level (ADR 0026 D11).
pub const DEFAULT_THINKING_LEVEL: &str = "medium";

/// One thinking map: level → wire value; a `None` wire value is pi's `null` — the level is
/// **not supported** and must not be offered. Absent from the map = unsupported.
pub type ThinkingMap = &'static [(&'static str, Option<&'static str>)];

/// The rank of a level name, or `None` for an unknown name.
pub fn thinking_rank(level: &str) -> Option<usize> {
    THINKING_LEVELS.iter().position(|entry| *entry == level)
}

/// The wire value a map gives a level, if the map supports it.
pub fn supported_wire(map: ThinkingMap, level: &str) -> Option<&'static str> {
    map.iter()
        .find(|(name, _)| *name == level)
        .and_then(|(_, wire)| *wire)
}

/// Clamps a requested level to the nearest **supported** one (ADR 0026 D11): first
/// the nearest supported level **up** the rank order, then — when nothing above is supported —
/// the nearest **down**. `None` when the map is absent (the caller then sends no parameter).
///
/// `deepseek-v4.1-flash` supports low/high/max, so `medium→high`, `minimal→low`, `xhigh→max`,
/// and `off→low` (it cannot switch thinking off).
pub fn clamp_thinking(
    map: Option<ThinkingMap>,
    requested: &str,
) -> Option<(&'static str, &'static str)> {
    clamp_thinking_with(map, requested, None)
}

/// The budget a wire value asks for, when it is `enabled:<budget>`.
pub fn wire_budget(wire: &str) -> Option<u32> {
    wire.strip_prefix("enabled:")?.parse().ok()
}

/// Whether a level's wire value fits the request's `max_tokens` (review B1). A budget the
/// **level** chose must satisfy the adapter's range (`1024 <= budget < max_tokens`); `None`
/// means "no cap". `adaptive` and "send nothing" always fit.
pub fn level_fits(wire: &str, max_tokens: Option<u32>) -> bool {
    let Some(budget) = wire_budget(wire) else {
        return true;
    };
    // A budget the level chose is **clamped** to `max_tokens - 1` (review B1), so it fits as
    // long as there is any room at all. Only `max_tokens <= MIN_THINKING_BUDGET` leaves no
    // valid budget, and then the level is unsupported.
    let _ = budget;
    match max_tokens {
        Some(max) => max > crate::MIN_THINKING_BUDGET,
        None => true,
    }
}

/// [`clamp_thinking`] with the request's `max_tokens`: a level whose **own** budget does not
/// fit is treated as unsupported, so the clamp walks past it (review B1). The budget itself is
/// clamped later, when the wire value is turned into the adapter's setting.
pub fn clamp_thinking_with(
    map: Option<ThinkingMap>,
    requested: &str,
    max_tokens: Option<u32>,
) -> Option<(&'static str, &'static str)> {
    let map = map?;
    let rank = thinking_rank(requested)
        .unwrap_or_else(|| thinking_rank(DEFAULT_THINKING_LEVEL).expect("the default is a level"));
    // Up first: the nearest supported level at or above the request.
    for level in &THINKING_LEVELS[rank..] {
        if let Some(wire) = supported_wire(map, level)
            && level_fits(wire, max_tokens)
        {
            return Some((level, wire));
        }
    }
    // Nothing above: the nearest supported level below.
    for level in THINKING_LEVELS[..rank].iter().rev() {
        if let Some(wire) = supported_wire(map, level)
            && level_fits(wire, max_tokens)
        {
            return Some((level, wire));
        }
    }
    None
}

/// The map a model uses: its own `thinkingLevelMap` first, the descriptor's provider-level map
/// second, and `None` (send nothing) when neither exists (ADR 0026 D11).
pub fn thinking_map_for(descriptor: &ProviderDescriptor, model: &str) -> Option<ThinkingMap> {
    if let Some(entry) = descriptor.model(model)
        && !entry.thinking.is_empty()
    {
        return Some(entry.thinking);
    }
    (!descriptor.thinking.is_empty()).then_some(descriptor.thinking)
}

/// The descriptor id a named provider's protocol maps to (ADR 0026 D10).
pub fn protocol_descriptor_id(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::OpenAiCompatible => GENERIC_ID,
        Protocol::Anthropic => "anthropic",
        // Unreachable: `Protocol::from_kind` never yields `Responses` (D10's named/custom
        // providers cannot declare it), so no `NamedProvider` ever carries this variant.
        Protocol::Responses => crate::oauth_chatgpt::PROVIDER_ID,
    }
}

/// The thinking map a [`ProviderRef`]'s model uses (ADR 0026 D11): a built-in descriptor's
/// model/provider map, or the map of the descriptor its kind names.
pub fn thinking_map_for_ref(provider: &ProviderRef, model: &str) -> Option<ThinkingMap> {
    match provider {
        ProviderRef::BuiltIn(descriptor) => thinking_map_for(descriptor, model),
        ProviderRef::Named(named) => {
            descriptor(protocol_descriptor_id(named.protocol)).and_then(|descriptor| {
                (!descriptor.thinking.is_empty()).then_some(descriptor.thinking)
            })
        }
    }
}

/// The levels a [`ProviderRef`]'s model supports, in rank order (ADR 0026 D11).
pub fn supported_thinking_levels_ref(provider: &ProviderRef, model: &str) -> Vec<&'static str> {
    supported_thinking_levels_ref_with(provider, model, None)
}

/// [`supported_thinking_levels_ref`] with the request's `max_tokens`: a level whose budget
/// does not fit is not offered (review B1), so the completion never lists a level the switch
/// would have to walk past.
pub fn supported_thinking_levels_ref_with(
    provider: &ProviderRef,
    model: &str,
    max_tokens: Option<u32>,
) -> Vec<&'static str> {
    let Some(map) = thinking_map_for_ref(provider, model) else {
        return Vec::new();
    };
    THINKING_LEVELS
        .iter()
        .filter(|level| supported_wire(map, level).is_some_and(|wire| level_fits(wire, max_tokens)))
        .copied()
        .collect()
}

/// The levels a model supports, in rank order — `/thinking`'s completion (ADR 0026 D11). Empty
/// when the model has no map: no parameter is ever sent, so nothing is offered.
pub fn supported_thinking_levels(descriptor_id: &str, model: &str) -> Vec<&'static str> {
    supported_thinking_levels_with(descriptor_id, model, None)
}

/// [`supported_thinking_levels`] with the request's `max_tokens` (review B1).
pub fn supported_thinking_levels_with(
    descriptor_id: &str,
    model: &str,
    max_tokens: Option<u32>,
) -> Vec<&'static str> {
    let Some(map) =
        crate::descriptor(descriptor_id).and_then(|descriptor| thinking_map_for(descriptor, model))
    else {
        return Vec::new();
    };
    THINKING_LEVELS
        .iter()
        .filter(|level| supported_wire(map, level).is_some_and(|wire| level_fits(wire, max_tokens)))
        .copied()
        .collect()
}

/// Parses a level name. The legacy Anthropic spellings are accepted for one version and the
/// caller warns. Returns `(level, legacy_budget, legacy)`: `adaptive` is the middle of the
/// scale, and a manual `enabled:<budget>` keeps the user's budget (ADR 0026 D11: the legacy
/// form still sends what the user asked for) while clamping to the level it resembles.
pub fn parse_thinking_level(raw: &str) -> Result<(&'static str, Option<u32>, bool), String> {
    if let Some(rank) = thinking_rank(raw) {
        return Ok((THINKING_LEVELS[rank], None, false));
    }
    match raw {
        "adaptive" => Ok(("medium", None, true)),
        other if other.starts_with("enabled:") => {
            let budget = other
                .trim_start_matches("enabled:")
                .parse::<u32>()
                .map_err(|_| {
                    format!("--thinking {other:?} needs a u32 budget, e.g. enabled:8192")
                })?;
            Ok(("high", Some(budget), true))
        }
        other => Err(format!(
            "--thinking {other:?} is not one of {} (or the legacy off|adaptive|enabled:<budget>)",
            THINKING_LEVELS.join("|")
        )),
    }
}

#[cfg(test)]
mod thinking_tests {
    use super::*;

    /// The verified deepseek-v4.1-flash map clamps **up** first (ADR 0026 D11).
    #[test]
    fn the_clamp_prefers_the_nearest_supported_level_above() {
        let map = thinking_map_for(descriptor("opencode-go").unwrap(), "deepseek-v4.1-flash");
        assert_eq!(clamp_thinking(map, "medium"), Some(("high", "high")));
        assert_eq!(clamp_thinking(map, "minimal"), Some(("low", "low")));
        assert_eq!(clamp_thinking(map, "xhigh"), Some(("max", "max")));
        // Nothing above is supported from `max`, so the clamp comes back **down**.
        assert_eq!(clamp_thinking(map, "off"), Some(("low", "low")));
    }

    /// The completion lists only the supported levels, in rank order (D11).
    #[test]
    fn the_completion_lists_only_supported_levels_in_rank_order() {
        assert_eq!(
            supported_thinking_levels("opencode-go", "deepseek-v4.1-flash"),
            vec!["low", "high", "max"]
        );
        // A model with no map anywhere offers nothing and sends nothing.
        assert!(supported_thinking_levels("opencode-go", "gpt-5-nano").is_empty());
    }

    /// A supported level passes through unchanged; an unknown name clamps from the default's
    /// rank.
    #[test]
    fn a_supported_level_passes_through() {
        let map = thinking_map_for(descriptor("opencode-go").unwrap(), "deepseek-v4.1-flash");
        assert_eq!(clamp_thinking(map, "low"), Some(("low", "low")));
        assert_eq!(clamp_thinking(map, "max"), Some(("max", "max")));
        assert_eq!(clamp_thinking(map, "nonsense"), Some(("high", "high")));
    }

    /// No map anywhere: no parameter at all (D11: no invented wire values).
    #[test]
    fn a_mapless_model_sends_nothing() {
        let map = thinking_map_for(descriptor("opencode-go").unwrap(), "gpt-5-nano");
        assert_eq!(clamp_thinking(map, "high"), None);
    }

    /// The legacy Anthropic spellings parse with the warning flag, and a manual
    /// `enabled:<budget>` keeps the user's budget (D11).
    #[test]
    fn the_legacy_spellings_still_parse() {
        assert_eq!(parse_thinking_level("adaptive"), Ok(("medium", None, true)));
        assert_eq!(parse_thinking_level("high"), Ok(("high", None, false)));
        assert_eq!(parse_thinking_level("off"), Ok(("off", None, false)));
        assert_eq!(
            parse_thinking_level("enabled:8192"),
            Ok(("high", Some(8192), true))
        );
        assert!(parse_thinking_level("enabled:not-a-number").is_err());
        assert!(parse_thinking_level("yolo").is_err());
    }

    /// A1: the anthropic map gives every level a **distinct** wire value, so two levels are
    /// never the same request, and the default (`medium`) is the provider's own `adaptive`.
    #[test]
    fn the_anthropic_levels_are_distinct() {
        let wires: Vec<&str> = THINKING_LEVELS
            .iter()
            .filter_map(|level| supported_wire(ANTHROPIC_THINKING, level))
            .collect();
        let mut unique = wires.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(wires.len(), 7, "all seven levels are supported");
        assert_eq!(unique.len(), 7, "every level has its own value: {wires:?}");
        assert_eq!(
            supported_wire(ANTHROPIC_THINKING, "medium"),
            Some("adaptive")
        );
        assert_ne!(
            supported_wire(ANTHROPIC_THINKING, "low"),
            supported_wire(ANTHROPIC_THINKING, "max")
        );
    }
}
