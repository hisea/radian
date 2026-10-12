use std::collections::HashSet;
use std::time::Duration;

use agent_core::{
    EventSink, Message, Model, ModelContext, ModelError, ModelRequest, ModelResponse,
};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client, Url, redirect};

use crate::config::{
    ConfigError, OpenAiCompatibleConfig, is_local_host, openai_host_is_default, redact_url,
};
use crate::request::build_body;
use crate::sse::SseParser;
use crate::stream::{Assembler, Progress};

/// How much of an error body is quoted in the error message.
const ERROR_BODY_BYTES: usize = 2048;

/// A model reached over an OpenAI-compatible Chat Completions API, streamed.
///
/// There is no retry: a failed request ends the turn with a `ModelError`, and the host decides
/// whether to try again.
#[derive(Clone)]
pub struct OpenAiCompatible {
    client: Client,
    endpoint: Url,
    headers: HeaderMap,
    config: OpenAiCompatibleConfig,
    /// Whether the last request asked for streamed usage (resolved from `include_usage`).
    include_usage: bool,
    /// The API key and custom header values, masked in every error message.
    secrets: Vec<String>,
}

impl std::fmt::Debug for OpenAiCompatible {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `secrets` is left out on purpose; `headers` marks its values sensitive.
        f.debug_struct("OpenAiCompatible")
            .field("endpoint", &redact_url(self.endpoint.as_str()))
            .field("headers", &self.headers)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl OpenAiCompatible {
    pub fn new(config: OpenAiCompatibleConfig) -> Result<Self, ConfigError> {
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
        if config.api_key.is_some() && base.scheme() == "http" && !is_local_host(&host) {
            return Err(ConfigError::InsecureKey { host });
        }
        // J8: explicit override wins; the default is on only for OpenAI's own endpoint.
        let include_usage = config
            .include_usage
            .unwrap_or_else(|| openai_host_is_default(&host));
        // `join` would drop the last path segment of a base without a trailing slash.
        let path = format!("{}/chat/completions", base.path().trim_end_matches('/'));
        base.set_path(&path);

        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        if let Some(key) = &config.api_key {
            let mut value = HeaderValue::from_str(&format!("Bearer {}", key.expose()))
                .map_err(|_| ConfigError::Header("Authorization".into()))?;
            value.set_sensitive(true);
            headers.insert(AUTHORIZATION, value);
        }
        for (name, value) in &config.headers {
            let header_name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| ConfigError::Header(name.clone()))?;
            let header_value =
                HeaderValue::from_str(value).map_err(|_| ConfigError::Header(name.clone()))?;
            let mut header_value = header_value;
            header_value.set_sensitive(true);
            headers.insert(header_name, header_value);
        }

        let client = Client::builder()
            .connect_timeout(config.connect_timeout)
            .user_agent(concat!("radian/", env!("CARGO_PKG_VERSION")))
            // A redirect would resend the request, and the key with it, somewhere else.
            .redirect(redirect::Policy::none())
            .build()
            .map_err(|e| ConfigError::Client(e.without_url().to_string()))?;
        let secrets = config
            .api_key
            .iter()
            .map(|key| key.expose().to_owned())
            .chain(config.headers.iter().map(|(_, value)| value.clone()))
            .filter(|secret| !secret.is_empty())
            .collect();
        Ok(Self {
            client,
            endpoint: base,
            headers,
            config,
            include_usage,
            secrets,
        })
    }

    /// Whether this adapter asks for streamed usage, after the host-based default policy.
    pub fn include_usage(&self) -> bool {
        self.include_usage
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

        let status = response.status();
        if !status.is_success() {
            let snippet = read_error_body(&mut response, self.config.idle_timeout).await;
            let mut message = format!("HTTP {status}: {snippet}");
            // J10: a 400 from a server that rejects the usage option deserves a way out.
            if status.as_u16() == 400 && self.include_usage {
                message.push_str(
                    "; the server rejected stream_options; set include_usage to Some(false) to \
                     turn the option off",
                );
            }
            // Throttling and server trouble are the provider's; the rest is the request's.
            return Err(
                if status.is_server_error() || matches!(status.as_u16(), 408 | 429) {
                    ModelError::Transport(message)
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
        assembler.finish(taken)
    }
}

impl Model for OpenAiCompatible {
    async fn complete(
        &self,
        request: ModelRequest,
        cx: ModelContext<'_>,
    ) -> Result<ModelResponse, ModelError> {
        let body = build_body(&request, &self.config, self.include_usage)?.to_string();
        let taken = tool_call_ids(&request.messages);
        // The core already drops this future on cancellation; this also covers a host that
        // calls `complete` directly.
        tokio::select! {
            biased;
            () = cx.cancellation.cancelled() => {
                Err(ModelError::Transport("the request was cancelled".into()))
            }
            result = self.run(body, taken, cx.events) => result.map_err(|e| self.mask(e)),
        }
    }
}

impl OpenAiCompatible {
    /// A server may echo the credentials it was sent in an error body or stream error; the
    /// host will log what we return, so mask them here.
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
        }
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
    // reqwest wraps the useful cause (connection refused, TLS, ...) in a generic message.
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        text = format!("{text}: {cause}");
        source = cause.source();
    }
    // Drop a URL if one is quoted; it may hold credentials.
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
