use std::fmt;
use std::time::Duration;

use serde_json::{Map, Value};

/// The default `anthropic-version` header value. Anthropic's versioning page states the
/// header is required and lists `2023-06-01` as the current version.
pub const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";

/// An API key that never shows up in `Debug` output. The adapter also masks it in the error
/// messages it produces, including text a server echoes back.
#[derive(Clone)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

/// How to reach an Anthropic `/v1/messages` endpoint.
///
/// The library never reads the environment; the host decides where the key comes from.
/// Authentication is API key only: there is no OAuth path.
#[derive(Clone)]
pub struct AnthropicConfig {
    /// Everything before `/v1/messages`, e.g. `https://api.anthropic.com`.
    pub base_url: String,
    pub model: String,
    pub api_key: Option<ApiKey>,
    /// The `anthropic-version` header. Defaults to [`DEFAULT_ANTHROPIC_VERSION`].
    pub anthropic_version: String,
    /// The `max_tokens` request field. Anthropic lists it without an `optional` qualifier and
    /// every official example carries it, so the adapter always sends it.
    pub max_tokens: u32,
    /// Extended thinking, off by default. When `Some`, the request carries the `thinking`
    /// object and the adapter preserves the response's `thinking`/`redacted_thinking` blocks
    /// (signatures included) in the assistant message's `provider_payload`, so a later tool
    /// turn echoes them back unchanged (ADR 0014 R10; wire notes §2.3/§4.2). `None` sends no
    /// `thinking` key and leaves the request byte-identical to the pre-thinking adapter.
    pub thinking: Option<ThinkingConfig>,
    /// Prompt caching, on by default ([`CacheMode::Auto`]). Under `None` the request carries no
    /// `cache_control` and is byte-identical to the adapter before prompt caching existed.
    pub cache: CacheMode,
    /// Sent with every request, after the built-in headers.
    pub headers: Vec<(String, String)>,
    /// Merged into the request body (e.g. `temperature`, `top_p`), under the per-request
    /// settings. `model`, `messages`, `max_tokens`, `stream`, `system` and `tools` cannot be
    /// set this way.
    pub extra_body: Map<String, Value>,
    pub connect_timeout: Duration,
    /// How long the server may stay silent mid-response before the request fails. There is no
    /// total time limit: a long answer is fine while bytes keep coming.
    pub idle_timeout: Duration,
    /// Cap on the text, thinking and tool-call argument bytes of one response. A server that
    /// never stops cannot grow host memory past it.
    pub max_response_bytes: usize,
}

/// Extended thinking (ADR 0014 R10). Enabling it is the host's decision; the adapter never
/// turns it on by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingConfig {
    /// Manual extended thinking: `{"type":"enabled","budget_tokens":N}`. The API requires
    /// `budget_tokens >= 1024` and `< max_tokens` (wire notes §2.3). Deprecated on Claude 4.6
    /// and rejected by Claude 4.7+; use [`ThinkingConfig::Adaptive`] on those models.
    Enabled { budget_tokens: u32 },
    /// Adaptive thinking: `{"type":"adaptive"}`. The model decides whether and how much to
    /// think; the recommended mode on Claude 4.6+ (wire notes §2.3).
    Adaptive,
}

/// Prompt caching (Anthropic `cache_control`, wire notes §2.4). The adapter places explicit
/// breakpoints itself; it never sends the top-level automatic-caching field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheMode {
    /// Place at most one breakpoint on each of: the last tool definition, the end of the system
    /// prompt, and the last cacheable block of the last message. That is three of the four
    /// documented slots; the adapter never uses the fourth (wire notes §2.4).
    #[default]
    Auto,
    /// No caching: no `cache_control` anywhere, so the request body is byte-identical to the
    /// adapter before prompt caching existed. A host that wants a stable request shape (or an
    /// endpoint that rejects `cache_control`) sets this.
    None,
}

/// Header values are hidden: custom headers often carry credentials such as `X-API-Key`.
impl fmt::Debug for AnthropicConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let header_names: Vec<&str> = self.headers.iter().map(|(name, _)| name.as_str()).collect();
        f.debug_struct("AnthropicConfig")
            .field("base_url", &redact_url(&self.base_url))
            .field("model", &self.model)
            .field("api_key", &self.api_key)
            .field("anthropic_version", &self.anthropic_version)
            .field("max_tokens", &self.max_tokens)
            .field("thinking", &self.thinking)
            .field("cache", &self.cache)
            .field("headers", &header_names)
            .field("extra_body", &self.extra_body)
            .field("connect_timeout", &self.connect_timeout)
            .field("idle_timeout", &self.idle_timeout)
            .field("max_response_bytes", &self.max_response_bytes)
            .finish()
    }
}

impl AnthropicConfig {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            api_key: None,
            anthropic_version: DEFAULT_ANTHROPIC_VERSION.into(),
            max_tokens: 4096,
            thinking: None,
            cache: CacheMode::Auto,
            headers: Vec::new(),
            extra_body: Map::new(),
            connect_timeout: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(120),
            max_response_bytes: 32 * 1024 * 1024,
        }
    }

    pub fn with_api_key(mut self, key: ApiKey) -> Self {
        self.api_key = Some(key);
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    pub fn with_thinking(mut self, thinking: ThinkingConfig) -> Self {
        self.thinking = Some(thinking);
        self
    }

    pub fn with_cache(mut self, cache: CacheMode) -> Self {
        self.cache = cache;
        self
    }

    pub fn with_anthropic_version(mut self, version: impl Into<String>) -> Self {
        self.anthropic_version = version.into();
        self
    }
}

/// Why an adapter could not be built.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("invalid base URL `{url}`: {reason}")]
    BaseUrl { url: String, reason: String },
    #[error(
        "refusing to send an API key over plain http to `{host}`; use https, or a local address"
    )]
    InsecureKey { host: String },
    #[error("invalid header `{0}`")]
    Header(String),
    #[error("could not build the HTTP client: {0}")]
    Client(String),
}

pub(crate) fn is_local_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
}

/// The URL without credentials: userinfo is dropped, query values and the fragment are
/// masked. A URL that does not parse is not echoed at all.
pub(crate) fn redact_url(raw: &str) -> String {
    let Ok(mut url) = reqwest::Url::parse(raw) else {
        return "<unparseable url>".into();
    };
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_fragment(None);
    if url.query().is_some() {
        let keys: Vec<String> = url.query_pairs().map(|(key, _)| key.into_owned()).collect();
        url.query_pairs_mut()
            .clear()
            .extend_pairs(keys.iter().map(|key| (key.as_str(), "<redacted>")));
    }
    url.to_string()
}
