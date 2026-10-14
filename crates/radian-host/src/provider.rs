//! Provider descriptors (ADR 0026 D0): one registered entry per provider, saying which wire
//! protocol each model uses, what base URL to talk to, how to log in, and which request
//! headers to add.
//!
//! The adapters (`agent-provider-*`) do not change: a descriptor only chooses which adapter to
//! build and what to pass it. The four built-ins are registered at compile time; there is no
//! dynamic plugin loading (v5 §7, ADR 0023 U8).

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

/// `opencode-go`'s built-in list, from `monotykamary/pi-opencode-go-provider@aec40ba`
/// `models.json` (accessed 2026-10-13): `api: "anthropic-messages"` → [`Protocol::Anthropic`],
/// everything else → [`Protocol::OpenAiCompatible`]. Six of the 31 models use Anthropic.
const OPENCODE_GO_MODELS: &[DescriptorModel] = &[
    go_openai("mimo-v2.6-pro", "MiMo-V2.6-Pro"),
    go_openai("mimo-v2.5", "MiMo V2.5"),
    go_openai("grok-4.7", "Grok 4.7"),
    go_openai("longcat-2.5-preview-free", "LongCat 2.5 Preview Free"),
    go_openai("glm-5.3-flash", "GLM-5.3-Flash"),
    go_anthropic("qwen3.8-max", "Qwen3.8 Max"),
    go_openai("kimi-k3", "Kimi K3"),
    DescriptorModel {
        thinking: DEEPSEEK_V41_FLASH_THINKING,
        ..go_openai("deepseek-v4.1-flash", "DeepSeek V4.1 Flash")
    },
    go_openai(
        "deepseek-v4-flash-vision-exp",
        "DeepSeek V4 Flash Vision Exp",
    ),
    go_openai("longcat-2.0", "LongCat-2.0"),
    go_openai("space-bunny", "Space Bunny"),
    go_anthropic("minimax-m2.7", "MiniMax-M2.7"),
    go_openai("mimo-v2.5-pro", "MiMo V2.5 Pro"),
    go_openai("mimo-v2.6-flash", "MiMo-V2.6-Flash"),
    go_anthropic("minimax-m3", "MiniMax-M3"),
    go_openai("gpt-5.6-luna", "GPT-5.6 Luna"),
    go_anthropic("qwen3.8-flash", "Qwen3.8 Flash"),
    go_openai("glm-5.2", "GLM-5.2"),
    go_openai("hy3", "Hy3"),
    go_openai("muse-spark-1.2-contributor", "Muse Spark 1.2 Contributor"),
    go_anthropic("claude-haiku-5-5", "Claude Haiku 5.5"),
    go_openai("gpt-6-luna", "GPT-6 Luna"),
    go_openai("deepseek-v4-pro", "DeepSeek V4 Pro (New)"),
    go_openai("hy4-preview", "Hy4 preview"),
    go_openai("muse-spark-1.3-contributor", "Muse Spark 1.3 Contributor"),
    go_openai("step-5-preview-free", "Step 5 Preview Free"),
    go_openai("glm-5.3", "GLM-5.3"),
    go_openai("kimi-k2.7-code", "Kimi K2.7 Code"),
    go_openai("grok-4.6", "Grok 4.6"),
    go_anthropic("qwen3.7-plus", "Qwen3.7 Plus"),
    go_openai("deepseek-v4-flash", "DeepSeek V4 Flash"),
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

/// The verified `thinkingLevelMap` of `deepseek-v4.1-flash` (ADR 0026 D11; source:
/// `monotykamary/pi-opencode-go-provider` `models.json`, local revision 6837adb). It supports
/// **low/high/max** and cannot switch thinking off, so `off`/`minimal`/`medium`/`xhigh` are
/// `null`. The clamp tests use this table.
pub const DEEPSEEK_V41_FLASH_THINKING: ThinkingMap = &[
    ("off", None),
    ("minimal", None),
    ("low", Some("low")),
    ("medium", None),
    ("high", Some("high")),
    ("xhigh", None),
    ("max", Some("max")),
];

/// The anthropic descriptor's provider-level map (ADR 0026 D11): the levels the adapter
/// already knows. `off` sends no `thinking` key; every other level sends `adaptive`, the
/// setting the Messages API accepts today. No per-level `enabled:<budget>` values are invented
/// (AGENTS.md): a manual budget has no verified level mapping.
pub const ANTHROPIC_THINKING: ThinkingMap = &[
    ("off", Some("")),
    ("minimal", Some("adaptive")),
    ("low", Some("adaptive")),
    ("medium", Some("adaptive")),
    ("high", Some("adaptive")),
    ("xhigh", Some("adaptive")),
    ("max", Some("adaptive")),
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
/// base URL into a [`Route`] (ADR 0026 D0).
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
    let generic = descriptor.id == GENERIC_ID;
    if !generic && base_url_override.is_some() {
        return Err(format!(
            "--base-url (or model.base_url) only applies to --provider {GENERIC_ID}; \
             {descriptor_id} uses {}",
            descriptor.default_base_url
        ));
    }
    if !generic && !header_args.is_empty() {
        return Err(format!(
            "--header (or model.headers) only applies to --provider {GENERIC_ID}; {descriptor_id} \
             sets its own request headers"
        ));
    }
    let base_url = if generic {
        // `--base-url`/config wins, then the credential entry's base URL, then the descriptor.
        base_url_override
            .map(str::to_owned)
            .or_else(|| auth_base_url.map(str::to_owned))
            .unwrap_or_else(|| descriptor.default_base_url.to_owned())
    } else {
        // A credential-level override (the entry's `env.RADIAN_BASE_URL`) wins for tests and
        // self-hosted endpoints; otherwise a per-model base URL when the descriptor lists one,
        // else the descriptor default.
        auth_base_url
            .map(str::to_owned)
            .or_else(|| {
                descriptor
                    .model(model)
                    .map(|entry| entry.base_url.to_owned())
            })
            .unwrap_or_else(|| descriptor.default_base_url.to_owned())
    };
    let mut headers = descriptor.request_headers(session_id);
    if generic {
        // The descriptor has no headers of its own, so the user's are all of them.
        headers.extend(header_args.iter().cloned());
    }
    Ok(Route {
        descriptor_id: descriptor.id,
        model: model.to_owned(),
        protocol: descriptor.protocol_for(model),
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
    let map = map?;
    let rank = thinking_rank(requested)
        .unwrap_or_else(|| thinking_rank(DEFAULT_THINKING_LEVEL).expect("the default is a level"));
    // Up first: the nearest supported level at or above the request.
    for level in &THINKING_LEVELS[rank..] {
        if let Some(wire) = supported_wire(map, level) {
            return Some((level, wire));
        }
    }
    // Nothing above: the nearest supported level below.
    for level in THINKING_LEVELS[..rank].iter().rev() {
        if let Some(wire) = supported_wire(map, level) {
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

/// The levels a model supports, in rank order — `/thinking`'s completion (ADR 0026 D11). Empty
/// when the model has no map: no parameter is ever sent, so nothing is offered.
pub fn supported_thinking_levels(descriptor_id: &str, model: &str) -> Vec<&'static str> {
    let Some(map) =
        crate::descriptor(descriptor_id).and_then(|descriptor| thinking_map_for(descriptor, model))
    else {
        return Vec::new();
    };
    THINKING_LEVELS
        .iter()
        .filter(|level| supported_wire(map, level).is_some())
        .copied()
        .collect()
}

/// Parses a level name. The legacy Anthropic spellings are accepted for one version and the
/// caller warns: `adaptive` is the middle of the scale, and a manual `enabled:<budget>` has no
/// level of its own (the budget now comes from the provider map), so it clamps like `high`.
pub fn parse_thinking_level(raw: &str) -> Result<(&'static str, bool), String> {
    if let Some(rank) = thinking_rank(raw) {
        return Ok((THINKING_LEVELS[rank], false));
    }
    match raw {
        "adaptive" => Ok(("medium", true)),
        other if other.starts_with("enabled:") => Ok(("high", true)),
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

    /// The legacy Anthropic spellings parse with the warning flag.
    #[test]
    fn the_legacy_spellings_still_parse() {
        assert_eq!(parse_thinking_level("adaptive"), Ok(("medium", true)));
        assert_eq!(parse_thinking_level("high"), Ok(("high", false)));
        assert_eq!(parse_thinking_level("off"), Ok(("off", false)));
        assert_eq!(parse_thinking_level("enabled:8192"), Ok(("high", true)));
        assert!(parse_thinking_level("yolo").is_err());
    }
}
