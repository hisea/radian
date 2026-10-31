use std::fmt;
use std::time::Duration;

use serde_json::{Map, Value};

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

/// How to reach an OpenAI-compatible `/chat/completions` endpoint.
///
/// The library never reads the environment; the host decides where the key comes from.
#[derive(Clone)]
pub struct OpenAiCompatibleConfig {
    /// Everything before `/chat/completions`, e.g. `https://api.openai.com/v1`.
    pub base_url: String,
    pub model: String,
    pub api_key: Option<ApiKey>,
    /// Sent with every request, after the built-in headers.
    pub headers: Vec<(String, String)>,
    /// Merged into the request body (e.g. `temperature`, `max_tokens`), under the per-request
    /// settings. `model`, `messages`, `tools` and `stream` cannot be set this way.
    pub extra_body: Map<String, Value>,
    pub connect_timeout: Duration,
    /// How long the server may stay silent mid-response before the request fails. There is no
    /// total time limit: a long answer is fine while bytes keep coming.
    pub idle_timeout: Duration,
    /// Cap on the text and tool-call arguments of one response. A server that never stops
    /// cannot grow host memory past it.
    pub max_response_bytes: usize,
    /// Whether the request asks the server to include usage in the stream.
    /// `None` = the default policy (on for `api.openai.com`, off everywhere else),
    /// `Some(b)` = explicit override.
    pub include_usage: Option<bool>,
    /// The output cap the request sends as `max_tokens` (ADR 0018), from
    /// [`catalog_max_tokens`]: `min(limit.output, ⌊C/4⌋)` when the route knows both,
    /// `min(16384, ⌊C/4⌋)` when it knows only the window. `None` sends no field. An explicit
    /// `max_tokens` in `extra_body` or in the request settings wins and is never overwritten.
    pub max_tokens: Option<u32>,
    /// The unified thinking level's wire value, sent as `reasoning_effort` (ADR 0026 D11).
    /// `None` sends no key, which is byte-identical to the adapter before the feature.
    pub reasoning_effort: Option<String>,
    /// The assistant-message field the model requires its reasoning to be replayed in
    /// (ADR 0014 R4). `None` sends no reasoning at all, byte-identical to the
    /// adapter before the feature; `Some(field)` writes the reasoning of every assistant
    /// message under that key (an empty string when the message had none, Q3).
    pub reasoning_replay: Option<String>,
}

/// Header values are hidden: custom headers often carry credentials such as `X-API-Key`.
impl fmt::Debug for OpenAiCompatibleConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let header_names: Vec<&str> = self.headers.iter().map(|(name, _)| name.as_str()).collect();
        f.debug_struct("OpenAiCompatibleConfig")
            .field("base_url", &redact_url(&self.base_url))
            .field("model", &self.model)
            .field("api_key", &self.api_key)
            .field("headers", &header_names)
            .field("extra_body", &self.extra_body)
            .field("connect_timeout", &self.connect_timeout)
            .field("idle_timeout", &self.idle_timeout)
            .field("max_response_bytes", &self.max_response_bytes)
            .field("include_usage", &self.include_usage)
            .field("max_tokens", &self.max_tokens)
            .field("reasoning_effort", &self.reasoning_effort)
            .field("reasoning_replay", &self.reasoning_replay)
            .finish()
    }
}

impl OpenAiCompatibleConfig {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            api_key: None,
            headers: Vec::new(),
            extra_body: Map::new(),
            connect_timeout: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(120),
            max_response_bytes: 32 * 1024 * 1024,
            include_usage: None,
            max_tokens: None,
            reasoning_effort: None,
            reasoning_replay: None,
        }
    }

    pub fn with_api_key(mut self, key: ApiKey) -> Self {
        self.api_key = Some(key);
        self
    }

    pub fn with_include_usage(mut self, enabled: bool) -> Self {
        self.include_usage = Some(enabled);
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

/// The default for `include_usage`: only OpenAI's own endpoint reports usage compatibly.
/// Compared by exact host, never by substring: `evil-openai.com.attacker.net` must not match.
/// A trailing DNS dot names the same host, so `api.openai.com.` matches too.
pub(crate) fn openai_host_is_default(host: &str) -> bool {
    host.trim_end_matches('.')
        .eq_ignore_ascii_case("api.openai.com")
}

/// The cap a chat request's `max_tokens` takes when the route knows its window `C` but not the
/// model's output ceiling (ADR 0018). It is the host's output reservation cap too, so the
/// value sent is exactly the `R` the host already reserves and `B` does not change.
pub const WINDOW_ONLY_OUTPUT_CAP: u64 = 16_384;

/// The `max_tokens` an OpenAI-compatible chat request sends (ADR 0018): `min(O, ⌊C/4⌋)` when
/// the route knows both the context window `C` and the output ceiling `O`,
/// `min(WINDOW_ONLY_OUTPUT_CAP, ⌊C/4⌋)` when it knows only `C`, and no field when `C` is
/// unknown. A zero or missing number is unknown. Never returns zero.
pub fn catalog_max_tokens(context: Option<u64>, output: Option<u64>) -> Option<u32> {
    let context = context.filter(|value| *value > 0)?;
    let ceiling = output
        .filter(|value| *value > 0)
        .unwrap_or(WINDOW_ONLY_OUTPUT_CAP);
    let cap = ceiling.min(context / 4);
    (cap > 0).then(|| cap.min(u64::from(u32::MAX)) as u32)
}

/// The URL without credentials: userinfo is dropped, query values and the fragment are
/// masked. A URL that does not parse is not echoed at all.
///
/// Public because a host that names the configured endpoint in an error must redact it the
/// same way (ADR 0013 T4); `agent-provider-anthropic` has an identical copy.
pub fn redact_url(raw: &str) -> String {
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
