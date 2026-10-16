use std::fmt;
use std::time::Duration;

use serde_json::{Map, Value};

/// How to reach an OpenAI Responses endpoint.
///
/// The library never reads the environment and knows nothing about authentication: the caller
/// passes every header (the access token as `Authorization: Bearer …`, `chatgpt-account-id`,
/// `OpenAI-Beta: responses=experimental` and `originator`) in [`Self::headers`]. This keeps the
/// OAuth/refresh machinery in the host (ADR 0026).
#[derive(Clone)]
pub struct OpenAiResponsesConfig {
    /// Everything before `/responses`, e.g. `https://chatgpt.com/backend-api/codex`.
    pub base_url: String,
    pub model: String,
    /// Sent with every request, after the built-in `content-type` / `accept`. Values are
    /// treated as secrets and masked in every error message.
    pub headers: Vec<(String, String)>,
    /// Merged into the request body (e.g. `temperature`), under the per-request settings.
    /// The keys the adapter owns (`model`, `input`, `instructions`, `tools`, `stream`, `store`,
    /// `include`) cannot be set this way.
    pub extra_body: Map<String, Value>,
    pub connect_timeout: Duration,
    /// How long the server may stay silent mid-response before the request fails. There is no
    /// total time limit: a long answer is fine while bytes keep coming.
    pub idle_timeout: Duration,
    /// Cap on the text, reasoning and tool-call argument bytes of one response. A server that
    /// never stops cannot grow host memory past it.
    pub max_response_bytes: usize,
    /// The unified thinking level's wire value, sent as `reasoning.effort` (ADR 0026 D11).
    /// `None` sends no `reasoning` key. A per-request `reasoning_effort` setting wins over it
    /// (ADR 0018).
    pub reasoning_effort: Option<String>,
    /// The `reasoning.summary` value (e.g. `"auto"`), which makes the server stream reasoning
    /// summaries. `None` sends no summary request.
    pub reasoning_summary: Option<String>,
}

/// Header values are hidden: they carry the access token.
impl fmt::Debug for OpenAiResponsesConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let header_names: Vec<&str> = self.headers.iter().map(|(name, _)| name.as_str()).collect();
        f.debug_struct("OpenAiResponsesConfig")
            .field("base_url", &redact_url(&self.base_url))
            .field("model", &self.model)
            .field("headers", &header_names)
            .field("extra_body", &self.extra_body)
            .field("connect_timeout", &self.connect_timeout)
            .field("idle_timeout", &self.idle_timeout)
            .field("max_response_bytes", &self.max_response_bytes)
            .field("reasoning_effort", &self.reasoning_effort)
            .field("reasoning_summary", &self.reasoning_summary)
            .finish()
    }
}

impl OpenAiResponsesConfig {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            headers: Vec::new(),
            extra_body: Map::new(),
            connect_timeout: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(120),
            max_response_bytes: 32 * 1024 * 1024,
            reasoning_effort: None,
            reasoning_summary: None,
        }
    }

    /// Appends one request header. The caller supplies `Authorization`, `chatgpt-account-id`,
    /// `OpenAI-Beta` and `originator` (ADR 0026).
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

/// Why an adapter could not be built.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("invalid base URL `{url}`: {reason}")]
    BaseUrl { url: String, reason: String },
    #[error("refusing to send headers over plain http to `{host}`; use https, or a local address")]
    InsecureHeaders { host: String },
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
