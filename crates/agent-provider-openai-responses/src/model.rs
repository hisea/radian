use std::collections::HashSet;
use std::time::Duration;

use radian_core::{
    CANCELLATION_MESSAGE, EventSink, Message, Model, ModelContext, ModelError, ModelRequest,
    ModelResponse, RateLimits,
};
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client, Url, redirect};

use crate::config::{ConfigError, OpenAiResponsesConfig, is_local_host, redact_url};
use crate::overflow::classify_overflow;
use crate::request::build_body;
use crate::stream::{Assembler, Progress};
use agent_provider_common::usage_limit::chatgpt::usage_limit;
use agent_provider_common::{SseParser, retry_after};

/// How much of an error body is quoted in the error message.
const ERROR_BODY_BYTES: usize = 2048;

/// The header prefixes the codex endpoint reports its subscription rate-limit windows in
/// (`x-codex-primary-used-percent`, `x-codex-secondary-reset-at`, …). Kept raw on the response
/// for a later slice (ADR 0026; ADR 0027 slice ④).
const RATE_LIMIT_PREFIXES: [&str; 2] = ["x-codex-primary", "x-codex-secondary"];

/// A model reached over an OpenAI Responses API, streamed.
///
/// The adapter itself never retries: it classifies a failure on the error (`is_transient`,
/// ADR 0017) and the core decides whether to send the same request again.
#[derive(Clone)]
pub struct OpenAiResponses {
    client: Client,
    endpoint: Url,
    headers: HeaderMap,
    config: OpenAiResponsesConfig,
    /// The header values, masked in every error message.
    secrets: Vec<String>,
}

impl std::fmt::Debug for OpenAiResponses {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `secrets` is left out on purpose; `headers` marks its values sensitive.
        f.debug_struct("OpenAiResponses")
            .field("endpoint", &redact_url(self.endpoint.as_str()))
            .field("headers", &self.headers)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// Builds the HTTP client every [`OpenAiResponses`] sends through: the connect timeout the
/// caller passes, the `radian/<version>` user agent, and no redirects (a redirect would
/// resend the request, and the access token with it, elsewhere). [`OpenAiResponses::new`]
/// builds this; a caller that supplies its own client ([`OpenAiResponses::with_client`], e.g.
/// `ChatGptModel`, which keeps one client across calls) builds it here too, so the supplied
/// client carries exactly these settings.
pub fn build_client(connect_timeout: Duration) -> Result<Client, ConfigError> {
    Client::builder()
        .connect_timeout(connect_timeout)
        .user_agent(concat!("radian/", env!("CARGO_PKG_VERSION")))
        .redirect(redirect::Policy::none())
        .build()
        .map_err(|e| ConfigError::Client(e.without_url().to_string()))
}

impl OpenAiResponses {
    pub fn new(config: OpenAiResponsesConfig) -> Result<Self, ConfigError> {
        let client = build_client(config.connect_timeout)?;
        Self::with_client(config, client)
    }

    /// [`Self::new`], but over a caller-supplied client built by [`build_client`]. Every
    /// other guarantee is the same: the base URL is validated, plain http refuses to carry
    /// headers off-box, and header values are masked in errors.
    pub fn with_client(config: OpenAiResponsesConfig, client: Client) -> Result<Self, ConfigError> {
        let (endpoint, headers) = validated_endpoint_and_headers(&config)?;
        let secrets = secrets(&config.headers);
        Ok(Self {
            client,
            endpoint,
            headers,
            config,
            secrets,
        })
    }

    async fn run(
        &self,
        body: String,
        taken: HashSet<String>,
        events: &dyn EventSink,
    ) -> Result<ModelResponse, ModelError> {
        let mut response = self
            .client
            .post(self.endpoint.clone())
            .headers(self.headers.clone())
            .body(body)
            .send()
            .await
            .map_err(|e| transport(&e))?;

        // Read the rate-limit headers before the body: both consume the response.
        let limits = rate_limits(&response);
        let status = response.status();
        if !status.is_success() {
            let retry_after = retry_after(response.headers());
            let snippet = read_error_body(&mut response, self.config.idle_timeout).await;
            let message = format!("HTTP {status}: {snippet}");
            // ADR 0018: a recognizable context-overflow error is its own kind.
            if classify_overflow(status.as_u16(), &snippet) {
                return Err(ModelError::ContextOverflow(message));
            }
            // A used-up ChatGPT plan window is not throttling: the same request cannot succeed
            // until the window resets, so it is **not** retried (ADR 0017 retries
            // only a transient error, and `UsageLimit` is not one).
            if let Some(error) = usage_limit(status.as_u16(), &snippet) {
                return Err(error);
            }
            // Throttling and server trouble are temporary (ADR 0017).
            return Err(
                if status.is_server_error() || matches!(status.as_u16(), 408 | 429) {
                    ModelError::Unavailable {
                        message,
                        retry_after,
                        status: Some(status.as_u16()),
                    }
                } else {
                    ModelError::Protocol(message)
                },
            );
        }

        let mut parser = SseParser::default();
        let mut assembler = Assembler::new(events, self.config.max_response_bytes);
        'stream: loop {
            let chunk = tokio::time::timeout(self.config.idle_timeout, response.chunk())
                .await
                .map_err(|_| {
                    ModelError::Transport(format!(
                        "the server sent nothing for {:?}",
                        self.config.idle_timeout
                    ))
                })?
                .map_err(|e| transport(&e))?;
            let Some(chunk) = chunk else { break };
            for data in parser.push(&chunk)? {
                if let Progress::Done = assembler.feed(&data)? {
                    break 'stream;
                }
            }
        }
        if let Some(data) = parser.finish()? {
            assembler.feed(&data)?;
        }
        let mut response = assembler.finish(taken)?;
        response.limits = limits;
        Ok(response)
    }
}

/// The endpoint (`<base>/responses`) and the request headers for `config`, validated the
/// same way for [`OpenAiResponses::new`] and [`OpenAiResponses::with_client`].
fn validated_endpoint_and_headers(
    config: &OpenAiResponsesConfig,
) -> Result<(Url, HeaderMap), ConfigError> {
    let mut base = Url::parse(&config.base_url).map_err(|e| ConfigError::BaseUrl {
        url: redact_url(&config.base_url),
        reason: e.to_string(),
    })?;
    if !matches!(base.scheme(), "http" | "https") {
        return Err(ConfigError::BaseUrl {
            url: redact_url(&config.base_url),
            reason: "the scheme must be http or https".into(),
        });
    }
    let host = base.host_str().unwrap_or_default().to_owned();
    if host.is_empty() {
        return Err(ConfigError::BaseUrl {
            url: redact_url(&config.base_url),
            reason: "there is no host".into(),
        });
    }
    // Every header here may carry the access token, so plain http is refused off-box.
    if !config.headers.is_empty() && base.scheme() == "http" && !is_local_host(&host) {
        return Err(ConfigError::InsecureHeaders { host });
    }
    // `join` would drop the last path segment of a base without a trailing slash.
    let path = format!("{}/responses", base.path().trim_end_matches('/'));
    base.set_path(&path);

    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
    for (name, value) in &config.headers {
        let header_name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| ConfigError::Header(name.clone()))?;
        let mut header_value =
            HeaderValue::from_str(value).map_err(|_| ConfigError::Header(name.clone()))?;
        header_value.set_sensitive(true);
        headers.insert(header_name, header_value);
    }
    Ok((base, headers))
}

impl Model for OpenAiResponses {
    async fn complete(
        &self,
        request: ModelRequest,
        cx: ModelContext<'_>,
    ) -> Result<ModelResponse, ModelError> {
        let body = build_body(&request, &self.config)?.to_string();
        let taken = tool_call_ids(&request.messages);
        // The core already drops this future on cancellation; this also covers a host that
        // calls `complete` directly.
        tokio::select! {
            biased;
            () = cx.cancellation.cancelled() => {
                Err(ModelError::Transport(CANCELLATION_MESSAGE.into()))
            }
            result = self.run(body, taken, cx.events) => result.map_err(|e| self.mask(e)),
        }
    }
}

impl OpenAiResponses {
    /// A server may echo a header value it was sent in an error body or stream error; the host
    /// will log what we return, so mask them here.
    fn mask(&self, error: ModelError) -> ModelError {
        let scrub = |mut text: String| {
            for secret in &self.secrets {
                text = text.replace(secret.as_str(), "<redacted>");
            }
            text
        };
        match error {
            ModelError::Transport(text) => ModelError::Transport(scrub(text)),
            ModelError::Protocol(text) => ModelError::Protocol(scrub(text)),
            ModelError::Unavailable {
                message,
                retry_after,
                status,
            } => ModelError::Unavailable {
                message: scrub(message),
                retry_after,
                status,
            },
            ModelError::ContextOverflow(text) => ModelError::ContextOverflow(scrub(text)),
            // The plan-window body may echo the key too; the reset wait is not text.
            ModelError::UsageLimit { message, resets_in } => ModelError::UsageLimit {
                message: scrub(message),
                resets_in,
            },
            // The core's own fixed string; it carries no provider text to scrub.
            ModelError::EmptyResponse => ModelError::EmptyResponse,
        }
    }
}

/// The values to mask in errors. Besides each header value (the full `Bearer …` string), the
/// credential part after a scheme prefix is masked too: a server usually echoes the bare token,
/// not the `Authorization` header value.
fn secrets(headers: &[(String, String)]) -> Vec<String> {
    let mut secrets = Vec::new();
    for (_, value) in headers {
        if value.is_empty() {
            continue;
        }
        secrets.push(value.clone());
        for prefix in ["Bearer ", "Basic "] {
            if let Some(token) = value.strip_prefix(prefix).map(str::trim)
                && !token.is_empty()
            {
                secrets.push(token.to_owned());
            }
        }
    }
    secrets
}

/// The raw `x-codex-primary-*` / `x-codex-secondary-*` headers, in arrival order. `None` when
/// none were sent. Nothing is parsed: the numbers are kept as text for a later slice.
fn rate_limits(response: &reqwest::Response) -> Option<RateLimits> {
    let mut headers = Vec::new();
    for (name, value) in response.headers() {
        let name = name.as_str();
        if RATE_LIMIT_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
            && let Ok(value) = value.to_str()
        {
            headers.push((name.to_owned(), value.to_owned()));
        }
    }
    if headers.is_empty() {
        None
    } else {
        Some(RateLimits { headers })
    }
}

fn tool_call_ids(messages: &[Message]) -> HashSet<String> {
    messages
        .iter()
        .flat_map(|message| match message {
            Message::Assistant { tool_calls, .. } => {
                tool_calls.iter().map(|c| c.id.clone()).collect::<Vec<_>>()
            }
            Message::Tool { result, .. } => vec![result.call_id.clone()],
            Message::User { .. } => Vec::new(),
        })
        .collect()
}

/// The URL is left out of the message: it could carry credentials.
fn transport(error: &reqwest::Error) -> ModelError {
    ModelError::Transport(format!("request failed: {}", describe(error)))
}

fn describe(error: &reqwest::Error) -> String {
    let mut text = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        text = format!("{text}: {cause}");
        source = cause.source();
    }
    match error.url() {
        Some(url) => text.replace(url.as_str(), "<url>"),
        None => text,
    }
}

async fn read_error_body(response: &mut reqwest::Response, idle: Duration) -> String {
    let mut body = Vec::new();
    while body.len() < ERROR_BODY_BYTES {
        match tokio::time::timeout(idle, response.chunk()).await {
            Ok(Ok(Some(chunk))) => body.extend_from_slice(&chunk),
            _ => break,
        }
    }
    body.truncate(ERROR_BODY_BYTES);
    let text = String::from_utf8_lossy(&body);
    let text = text.trim();
    if text.is_empty() {
        "(empty body)".into()
    } else {
        text.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_include_the_bare_bearer_token() {
        let secrets = secrets(&[("Authorization".into(), "Bearer sk-abc-123".into())]);
        assert!(secrets.contains(&"Bearer sk-abc-123".to_owned()));
        assert!(secrets.contains(&"sk-abc-123".to_owned()));
    }
}
