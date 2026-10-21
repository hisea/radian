use std::collections::HashSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent_core::{
    CANCELLATION_MESSAGE, EventSink, Message, Model, ModelContext, ModelError, ModelRequest,
    ModelResponse, RateLimits,
};
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, RETRY_AFTER};
use reqwest::{Client, Url, redirect};

use crate::config::{AnthropicConfig, ConfigError, is_local_host, redact_url};
use crate::overflow::classify_overflow;
use crate::request::build_body;
use crate::sse::SseParser;
use crate::stream::{Assembler, Progress};

/// How much of an error body is quoted in the error message.
const ERROR_BODY_BYTES: usize = 2048;

/// The response header prefix Anthropic reports its rate-limit windows under
/// (`anthropic-ratelimit-requests-limit`, `anthropic-ratelimit-tokens-remaining`,
/// `anthropic-ratelimit-requests-reset`, …). Kept raw on the response for the host's
/// `Headers` usage source (ADR 0027 slice ④); the exact names the host parses
/// are recorded in `crates/agent-provider-anthropic/WIRE.md`.
const RATE_LIMIT_PREFIX: &str = "anthropic-ratelimit-";

/// A model reached over the Anthropic Messages API, streamed, authenticated with an API key.
///
/// The adapter itself never retries: it classifies a failure on the error (`is_transient`,
/// ADR 0017) and the core decides whether to send the same request again. There is
/// no OAuth path (Anthropic's policy forbids routing consumer subscriptions through
/// third-party tools).
#[derive(Clone)]
pub struct Anthropic {
    client: Client,
    endpoint: Url,
    headers: HeaderMap,
    config: AnthropicConfig,
    /// The API key and custom header values, masked in every error message.
    secrets: Vec<String>,
}

impl std::fmt::Debug for Anthropic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `secrets` is left out on purpose; `headers` marks its values sensitive.
        f.debug_struct("Anthropic")
            .field("endpoint", &redact_url(self.endpoint.as_str()))
            .field("headers", &self.headers)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Anthropic {
    pub fn new(config: AnthropicConfig) -> Result<Self, ConfigError> {
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
        if config.anthropic_version.trim().is_empty() {
            return Err(ConfigError::Header("anthropic-version".into()));
        }
        // `join` would drop the last path segment of a base without a trailing slash.
        let path = format!("{}/v1/messages", base.path().trim_end_matches('/'));
        base.set_path(&path);

        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        headers.insert(
            HeaderName::from_static("anthropic-version"),
            HeaderValue::from_str(&config.anthropic_version)
                .map_err(|_| ConfigError::Header("anthropic-version".into()))?,
        );
        if let Some(key) = &config.api_key {
            let mut value = HeaderValue::from_str(key.expose())
                .map_err(|_| ConfigError::Header("x-api-key".into()))?;
            value.set_sensitive(true);
            headers.insert(HeaderName::from_static("x-api-key"), value);
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
            // Read `Retry-After` before the body: `read_error_body` consumes the response.
            let retry_after = retry_after(&response);
            let snippet = read_error_body(&mut response, self.config.idle_timeout).await;
            let message = format!("HTTP {status}: {snippet}");
            // ADR 0018: a recognizable context-overflow error is its own kind, so the host can
            // tell it apart from a generic bad request. The check is narrow: 400 with
            // `invalid_request_error` + "prompt is too long", or 413 `request_too_large`.
            if classify_overflow(status.as_u16(), &snippet) {
                return Err(ModelError::ContextOverflow(message));
            }
            // Throttling and server trouble are temporary (ADR 0017): the same
            // request may succeed if it is sent again, and the core retries it. The rest is
            // the request's own fault and is never retried.
            return Err(
                if status.is_server_error() || matches!(status.as_u16(), 408 | 429) {
                    ModelError::Unavailable {
                        message,
                        retry_after,
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

impl Model for Anthropic {
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

impl Anthropic {
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
            // The body may echo the key, so the text is scrubbed; the wait is not text.
            ModelError::Unavailable {
                message,
                retry_after,
            } => ModelError::Unavailable {
                message: scrub(message),
                retry_after,
            },
            // ADR 0018: an overflow body may echo the key too, so it is masked like the rest.
            ModelError::ContextOverflow(text) => ModelError::ContextOverflow(scrub(text)),
            // The core's own fixed string; it carries no provider text to scrub.
            ModelError::EmptyResponse => ModelError::EmptyResponse,
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

/// The raw `anthropic-ratelimit-*` headers, in arrival order. `None` when none were sent.
/// Nothing is parsed: the numbers are kept as text for the host's `Headers` usage source.
fn rate_limits(response: &reqwest::Response) -> Option<RateLimits> {
    let mut headers = Vec::new();
    for (name, value) in response.headers() {
        let name = name.as_str();
        if name.starts_with(RATE_LIMIT_PREFIX)
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

/// The wait the provider asked for in `Retry-After`, if the header is present and parseable.
/// Both forms HTTP allows are understood (RFC 7231 §7.1.3): a whole number of seconds, or an
/// IMF-fixdate. An unparseable value is ignored, and the core falls back to its own backoff.
fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    let value = response.headers().get(RETRY_AFTER)?;
    parse_retry_after(value.to_str().ok()?, SystemTime::now())
}

fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let date = parse_http_date(value)?;
    // A date in the past means "retry now"; the caller still backs off one step.
    Some(date.duration_since(now).unwrap_or_default())
}

/// `Sun, 06 Nov 1994 08:49:37 GMT` (IMF-fixdate). The weekday is ignored: a parser that
/// validates it has to do calendar arithmetic the sender may have got wrong, and the date
/// itself is what matters.
fn parse_http_date(value: &str) -> Option<SystemTime> {
    let rest = value.strip_suffix(" GMT")?;
    let (weekday, rest) = rest.split_once(", ")?;
    if weekday.len() != 3 {
        return None;
    }
    let mut parts = rest.split(' ');
    let day: i64 = parts.next()?.parse().ok()?;
    let month = month_number(parts.next()?)?;
    let year: i64 = parts.next()?.parse().ok()?;
    let time = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let (hour, rest) = time.split_once(':')?;
    let (minute, second) = rest.split_once(':')?;
    let hour: i64 = hour.parse().ok()?;
    let minute: i64 = minute.parse().ok()?;
    let second: i64 = second.parse().ok()?;
    if !(1..=31).contains(&day) || !(0..=23).contains(&hour) || !(0..=59).contains(&minute) {
        return None;
    }
    // 60 is a leap second.
    if !(0..=60).contains(&second) {
        return None;
    }
    let days = days_from_civil(year, month, day)?;
    let seconds = days
        .checked_mul(86_400)?
        .checked_add(hour * 3600 + minute * 60 + second)?;
    if seconds < 0 {
        return Some(UNIX_EPOCH);
    }
    UNIX_EPOCH.checked_add(Duration::from_secs(seconds as u64))
}

fn month_number(name: &str) -> Option<i64> {
    Some(match name {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    })
}

/// Days from 1970-01-01 to `year-month-day`, by Howard Hinnant's civil-calendar algorithm.
/// `None` for a month outside 1..=12.
fn days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
    if !(1..=12).contains(&month) {
        return None;
    }
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400; // [0, 399]
    let mp = if month > 2 { month - 3 } else { month + 9 }; // March = 0
    let doy = (153 * mp + 2) / 5 + day - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    Some(era * 146_097 + doe - 719_468)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(unix_seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(unix_seconds)
    }

    #[test]
    fn retry_after_seconds_are_taken_as_is() {
        assert_eq!(
            parse_retry_after("12", at(0)),
            Some(Duration::from_secs(12))
        );
        assert_eq!(
            parse_retry_after(" 3 ", at(0)),
            Some(Duration::from_secs(3))
        );
        assert_eq!(parse_retry_after("0", at(0)), Some(Duration::ZERO));
    }

    #[test]
    fn retry_after_accepts_an_imf_fixdate() {
        // The example date from RFC 7231 §7.1.1.1 is 784,111,777 seconds after the epoch.
        let target = 784_111_777;
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", at(target - 30)),
            Some(Duration::from_secs(30))
        );
        // A date already in the past means "now".
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", at(target + 5)),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn an_unparseable_retry_after_is_ignored() {
        for value in [
            "",
            "soon",
            "-1",
            "Sat, 06 Nov 1994 08:49:37 GMT", // a weekday is not the point; this parses
            "Sun, 06 Nov 1994 08:49:37",     // no `GMT`
            "Sun 06 Nov 1994 08:49:37 GMT",  // no comma
            "Sun, 06 Foo 1994 08:49:37 GMT", // unknown month
            "Sun, 06 Nov 1994 24:00:00 GMT",
            "Sun, 06 Nov 1994 08:61:00 GMT",
            "Sun, 00 Nov 1994 08:49:37 GMT",
            "Sun, 06 Nov 1994 08:49:37 GMT extra",
        ] {
            let parsed = parse_retry_after(value, at(0));
            // The one date with a wrong weekday is still a valid IMF-fixdate; the rest are not.
            if value.starts_with("Sat") {
                assert!(parsed.is_some(), "{value:?} must parse");
            } else {
                assert_eq!(parsed, None, "{value:?} must be ignored");
            }
        }
    }

    #[test]
    fn days_from_civil_matches_known_epochs() {
        assert_eq!(days_from_civil(1970, 1, 1), Some(0));
        assert_eq!(days_from_civil(1970, 1, 2), Some(1));
        assert_eq!(days_from_civil(1969, 12, 31), Some(-1));
        assert_eq!(days_from_civil(2000, 3, 1), Some(11_017));
        assert_eq!(days_from_civil(1994, 11, 6), Some(9_075));
    }
}
