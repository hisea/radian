use std::fmt;
use std::time::Duration;

use serde_json::{Map, Value};

/// An API key that never shows up in `Debug` output or error messages.
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
#[derive(Debug, Clone)]
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
        }
    }

    pub fn with_api_key(mut self, key: ApiKey) -> Self {
        self.api_key = Some(key);
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
