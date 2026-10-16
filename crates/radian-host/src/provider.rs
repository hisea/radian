//! Provider descriptors (ADR 0026 D0): one registered entry per provider, saying which wire
//! protocol each model uses, what base URL to talk to, how to log in, and which request
//! headers to add.
//!
//! The adapters (`agent-provider-*`) do not change: a descriptor only chooses which adapter to
//! build and what to pass it. The four built-ins are registered at compile time; there is no
//! dynamic plugin loading (v5 §7, ADR 0023 U8).

use std::collections::BTreeMap;

use crate::model::Provider;

/// Which wire protocol a model talks, mapped to one of the two existing adapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    OpenAiCompatible,
    Anthropic,
}

impl Protocol {
    /// The `--provider` name this protocol maps to ([`Provider::from_name`]).
    pub fn adapter_name(self) -> &'static str {
        match self {
            Self::OpenAiCompatible => "openai-compatible",
            Self::Anthropic => "anthropic",
        }
    }

    pub fn adapter(self) -> Provider {
        match self {
            Self::OpenAiCompatible => Provider::OpenAiCompatible,
            Self::Anthropic => Provider::Anthropic,
        }
    }
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

/// One model in a descriptor's built-in list. `protocol` and `base_url` are per model because
/// `opencode-go` serves both protocols on different paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DescriptorModel {
    pub id: &'static str,
    pub name: &'static str,
    pub protocol: Protocol,
    pub base_url: &'static str,
    /// The model's `thinkingLevelMap` (ADR 0026 D11): level → wire value, a `None` wire value
    /// is pi's `null` (unsupported). Empty = no model-level map; the provider's applies.
    pub thinking: ThinkingMap,
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
    /// The built-in model list (the full list is later merged with the cache and `/v1/models`,
    /// ADR 0026 D0).
    pub models: &'static [DescriptorModel],
    /// Extra request headers, expanded with the process's session id. See
    /// [`ProviderDescriptor::request_headers`].
    pub headers: fn(&str) -> Vec<(String, String)>,
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

    /// The protocol `model` uses: the built-in entry's protocol, or the descriptor's default
    /// for a model the list does not name (always known, never `None`).
    pub fn protocol_for(&self, model: &str) -> Protocol {
        self.model(model)
            .map_or(self.default_protocol, |entry| entry.protocol)
    }
}

/// The `auth.json` `env` key naming a named provider's wire protocol (ADR 0026 D10). pi
/// ignores it; Radian reads it when `[providers.<name>]` is absent. The default is the
/// generic `openai-compatible`.
pub const RADIAN_KIND: &str = "RADIAN_KIND";

impl Protocol {
    /// The wire protocol a `kind` names (D10: only the two adapters exist in this build).
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
    let mut choices: Vec<(String, String)> = descriptors()
        .iter()
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
        "a custom endpoint: name, protocol, base URL and key (ADR 0026 D10)".to_owned(),
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
    /// revision 1 Q2). `None` falls through to the catalog.
    pub reasoning_replay: Option<ReasoningReplayOverride>,
}

/// A `[providers.<id>]` override for the catalog's reasoning replay (ADR 0014 Q2):
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
    /// `reasoning_replay` (ADR 0014 Q2): a field name, or `false` to switch replay
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

    /// The wire protocol of `model`: the descriptor's choice, or the named provider's kind.
    pub fn protocol_for(&self, model: &str) -> Protocol {
        match self {
            Self::BuiltIn(descriptor) => descriptor.protocol_for(model),
            Self::Named(named) => named.protocol,
        }
    }

    /// The endpoint `model` talks to: per-model for a descriptor that lists it, else the
    /// descriptor default; a named provider has exactly one endpoint.
    pub fn base_url_for(&self, model: &str, auth_base_url: Option<&str>) -> String {
        match self {
            Self::BuiltIn(descriptor) => auth_base_url
                .map(str::to_owned)
                .or_else(|| {
                    descriptor
                        .model(model)
                        .map(|entry| entry.base_url.to_owned())
                })
                .unwrap_or_else(|| descriptor.default_base_url.to_owned()),
            Self::Named(named) => named.base_url.clone(),
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

/// The OpenAI-compatible base URL `opencode-go` serves, from the dogfood command and the
/// `opencode-go` extension's `DEFAULT_API.baseUrl`.
const OPENCODE_GO_BASE: &str = "https://opencode.ai/zen/go/v1";
/// The Anthropic-protocol base URL `opencode-go` serves (no `/v1`), from the extension's
/// `NPM_TO_API`.
const OPENCODE_GO_ANTHROPIC_BASE: &str = "https://opencode.ai/zen/go";
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

/// `opencode-go`'s built-in list, from `monotykamary/pi-opencode-go-provider@cdd8a70`
/// `models.json` (accessed 2026-10-14): `api: "anthropic-messages"` → [`Protocol::Anthropic`],
/// everything else → [`Protocol::OpenAiCompatible`]. Six of the 32 models use Anthropic, and
/// every `thinkingLevelMap` upstream carries is kept verbatim (ADR 0026 D11; the structural
/// test `opencode_go_models_match_upstream` reads the same fields from the same commit).
const OPENCODE_GO_MODELS: &[DescriptorModel] = &[
    go_openai("mimo-v2.6-pro", "MiMo-V2.6-Pro"),
    go_openai("mimo-v2.5", "MiMo V2.5"),
    DescriptorModel {
        thinking: THINKING_UP_TO_XHIGH,
        ..go_openai("grok-4.7", "Grok 4.7")
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
        ..go_openai("gpt-5.6-luna", "GPT-5.6 Luna")
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
        ..go_openai("muse-spark-1.2-contributor", "Muse Spark 1.2 Contributor")
    },
    DescriptorModel {
        thinking: THINKING_LOW_TO_MAX,
        ..go_anthropic("claude-haiku-5-5", "Claude Haiku 5.5")
    },
    DescriptorModel {
        thinking: THINKING_LOW_TO_MAX,
        ..go_openai("gpt-6-luna", "GPT-6 Luna")
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
        ..go_openai("muse-spark-1.3-contributor", "Muse Spark 1.3 Contributor")
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
    DescriptorModel {
        thinking: THINKING_UP_TO_XHIGH,
        ..go_openai("grok-4.6", "Grok 4.6")
    },
    go_anthropic("qwen3.7-plus", "Qwen3.7 Plus"),
    DescriptorModel {
        thinking: THINKING_LOW_HIGH_MAX,
        ..go_openai("deepseek-v4-flash", "DeepSeek V4 Flash")
    },
];

/// The models.dev cache's model names, kept as a small built-in fallback (the full list is
/// merged from the cache and `/v1/models`, ADR 0026 D0).
const ANTHROPIC_MODELS: &[DescriptorModel] = &[
    anthropic_model("claude-opus-5-5", "Claude Opus 5.5"),
    anthropic_model("claude-sonnet-4-5", "Claude Sonnet 4.5 (latest)"),
    anthropic_model("claude-haiku-4-5", "Claude Haiku 4.5 (latest)"),
    anthropic_model("claude-opus-4-5", "Claude Opus 4.5 (latest)"),
];

const OPENAI_MODELS: &[DescriptorModel] = &[
    openai_model("gpt-5.4", "GPT-5.4"),
    openai_model("gpt-5.4-pro", "GPT-5.4 Pro"),
    openai_model("gpt-5.5-pro", "GPT-5.5 Pro"),
    openai_model("gpt-3.5-turbo", "GPT-3.5-turbo"),
];

const fn model(
    id: &'static str,
    name: &'static str,
    protocol: Protocol,
    base_url: &'static str,
) -> DescriptorModel {
    DescriptorModel {
        id,
        name,
        protocol,
        base_url,
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
/// names `enabled:<budget>` but gives no per-level budgets — and are marked for the owner.
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
    model(id, name, Protocol::OpenAiCompatible, OPENCODE_GO_BASE)
}

const fn go_anthropic(id: &'static str, name: &'static str) -> DescriptorModel {
    model(id, name, Protocol::Anthropic, OPENCODE_GO_ANTHROPIC_BASE)
}

const fn anthropic_model(id: &'static str, name: &'static str) -> DescriptorModel {
    model(id, name, Protocol::Anthropic, ANTHROPIC_BASE)
}

const fn openai_model(id: &'static str, name: &'static str) -> DescriptorModel {
    model(id, name, Protocol::OpenAiCompatible, OPENAI_BASE)
}

/// The four built-in descriptors (ADR 0026 D0), in a stable order.
pub const DESCRIPTORS: &[ProviderDescriptor] = &[
    ProviderDescriptor {
        id: "opencode-go",
        display_name: "OpenCode Go",
        default_base_url: OPENCODE_GO_BASE,
        login: LoginMethod::ApiKey { extra_fields: &[] },
        thinking: &[],
        default_protocol: Protocol::OpenAiCompatible,
        models: OPENCODE_GO_MODELS,
        headers: opencode_go_headers,
    },
    ProviderDescriptor {
        id: "anthropic",
        display_name: "Anthropic",
        default_base_url: "https://api.anthropic.com",
        login: LoginMethod::ApiKey { extra_fields: &[] },
        thinking: ANTHROPIC_THINKING,
        default_protocol: Protocol::Anthropic,
        models: ANTHROPIC_MODELS,
        headers: no_headers,
    },
    ProviderDescriptor {
        id: "openai",
        display_name: "OpenAI",
        default_base_url: "https://api.openai.com/v1",
        login: LoginMethod::ApiKey { extra_fields: &[] },
        thinking: &[],
        default_protocol: Protocol::OpenAiCompatible,
        models: OPENAI_MODELS,
        headers: no_headers,
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
        // The user supplies the model id; there is no built-in list.
        models: &[],
        headers: no_headers,
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
    )
}

/// [`resolve_route`] for a [`ProviderRef`]: a named provider's endpoint and protocol come
/// from its declaration, so `--base-url`/`--header` (and config `model.base_url`/`headers`)
/// are refused for it too — they are the generic provider's dials (ADR 0026 D10).
pub fn resolve_route_for(
    provider: &ProviderRef,
    model: &str,
    base_url_override: Option<&str>,
    auth_base_url: Option<&str>,
    header_args: &[(String, String)],
    session_id: &str,
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
    let base_url = if generic {
        // `--base-url`/config wins, then the credential entry's base URL, then the descriptor.
        base_url_override
            .map(str::to_owned)
            .or_else(|| auth_base_url.map(str::to_owned))
            .unwrap_or_else(|| {
                ProviderRef::BuiltIn(descriptor(GENERIC_ID).unwrap()).base_url_for(model, None)
            })
    } else {
        provider.base_url_for(model, auth_base_url)
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
        protocol: provider.protocol_for(model),
        base_url,
        headers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_four_built_ins_are_registered() {
        let ids: Vec<&str> = descriptors().iter().map(|d| d.id).collect();
        assert_eq!(
            ids,
            ["opencode-go", "anthropic", "openai", "openai-compatible"]
        );
        assert!(descriptor("nope").is_none());
        assert_eq!(
            descriptor("anthropic").unwrap().default_base_url,
            "https://api.anthropic.com"
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
    fn opencode_go_maps_models_to_both_protocols() {
        let go = descriptor("opencode-go").unwrap();
        assert_eq!(
            go.protocol_for("deepseek-v4.1-flash"),
            Protocol::OpenAiCompatible
        );
        assert_eq!(go.protocol_for("claude-haiku-5-5"), Protocol::Anthropic);
        assert_eq!(go.protocol_for("qwen3.7-plus"), Protocol::Anthropic);
        assert_eq!(go.protocol_for("mimo-v2.6-pro"), Protocol::OpenAiCompatible);
        // A model the list does not name uses the descriptor default (openai-compatible).
        assert_eq!(go.protocol_for("not-a-model"), Protocol::OpenAiCompatible);
        // The Anthropic models point at the `/zen/go` base, the others at `/zen/go/v1`.
        assert_eq!(
            go.model("claude-haiku-5-5").unwrap().base_url,
            "https://opencode.ai/zen/go"
        );
        assert_eq!(
            go.model("deepseek-v4.1-flash").unwrap().base_url,
            "https://opencode.ai/zen/go/v1"
        );
        // The Anthropic protocol maps to the Anthropic adapter.
        assert_eq!(Protocol::Anthropic.adapter_name(), "anthropic");
        assert_eq!(Protocol::Anthropic.adapter(), Provider::Anthropic);
    }

    #[test]
    fn a_route_picks_the_adapter_base_url_and_headers() {
        // opencode-go: an openai-compatible model on /zen/go/v1, with the session and client
        // headers.
        let route =
            resolve_route("opencode-go", "deepseek-v4.1-flash", None, None, &[], "s1").unwrap();
        assert_eq!(route.protocol, Protocol::OpenAiCompatible);
        assert_eq!(route.base_url, "https://opencode.ai/zen/go/v1");
        assert_eq!(
            route.headers,
            vec![
                ("x-opencode-session".to_owned(), "s1".to_owned()),
                ("x-opencode-client".to_owned(), "radian".to_owned())
            ]
        );
        // Its Anthropic model goes to /zen/go with the Anthropic adapter.
        let anthropic =
            resolve_route("opencode-go", "claude-haiku-5-5", None, None, &[], "s1").unwrap();
        assert_eq!(anthropic.protocol, Protocol::Anthropic);
        assert_eq!(anthropic.base_url, "https://opencode.ai/zen/go");
        // A model the list does not name uses the descriptor default.
        let unknown_model =
            resolve_route("opencode-go", "future-model", None, None, &[], "s1").unwrap();
        assert_eq!(unknown_model.protocol, Protocol::OpenAiCompatible);
    }

    #[test]
    fn only_the_generic_descriptor_accepts_a_base_url_or_headers() {
        let headers = vec![("x-custom".to_owned(), "1".to_owned())];
        // A non-generic descriptor refuses both.
        assert!(
            resolve_route("opencode-go", "m", Some("https://x"), None, &[], "s")
                .unwrap_err()
                .contains("--base-url")
        );
        assert!(
            resolve_route("anthropic", "m", None, None, &headers, "s")
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
        )
        .unwrap();
        assert_eq!(with_override.base_url, "https://cli");
        let with_auth =
            resolve_route(GENERIC_ID, "m", None, Some("https://auth"), &[], "s").unwrap();
        assert_eq!(with_auth.base_url, "https://auth");
        let default = resolve_route(GENERIC_ID, "m", None, None, &[], "s").unwrap();
        assert_eq!(default.base_url, "https://api.openai.com/v1");
        // An unknown descriptor is refused with the list.
        let error = resolve_route("nope", "m", None, None, &[], "s").unwrap_err();
        assert!(error.contains("unknown provider"), "{error}");
        assert!(error.contains("opencode-go"), "{error}");
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
        assert_eq!(provider.base_url_for("m", None), "https://corp.example/v1");
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
        assert_eq!(provider.base_url_for("m", None), "https://auth.example/v1");
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
        assert_eq!(provider.base_url_for("m", None), "https://config.example");
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
        let route = resolve_route_for(&provider, "m1", None, None, &[], "s1").unwrap();
        assert_eq!(route.base_url, "https://corp.example/v1");
        assert_eq!(route.protocol, Protocol::OpenAiCompatible);
        assert!(route.headers.is_empty(), "a named provider has no headers");
        // The generic-only dials are refused.
        assert!(
            resolve_route_for(&provider, "m", Some("https://x"), None, &[], "s")
                .unwrap_err()
                .contains("--base-url")
        );
        let headers = vec![("x".to_owned(), "1".to_owned())];
        assert!(
            resolve_route_for(&provider, "m", None, None, &headers, "s")
                .unwrap_err()
                .contains("--header")
        );
    }
}

// ---------- the unified thinking levels (ADR 0026 D11) ----------

/// The seven unified thinking levels, in **rank order**. The clamp walks this order, so the
/// array order is load-bearing.
pub const THINKING_LEVELS: &[&str] = &["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// The default level (ADR 0026 D11, owner pick A, matching pi's `defaultThinkingLevel`).
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

/// Clamps a requested level to the nearest **supported** one (ADR 0026 D11, owner rule): first
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

    /// The verified deepseek-v4.1-flash map clamps **up** first (ADR 0026 D11, owner rule).
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
