//! The opencode-go usage source (ADR 0027 C4; the descriptor's `usage = Endpoint`).
//!
//! `GET https://opencode.ai/zen/go/v1/usage` with the key from `auth.json` (ADR 0026 D2),
//! every 60 s by default (15 s–10 min, or off). It runs only when the current or a logged-in
//! provider is opencode-go. The API reports **used** percentages; the snapshot carries the
//! **remaining** `100 − percent`, labeled, so the number cannot be misread (ADR 0027 C4).
//!
//! The request, the key handling, the period bounds and the two `resetsAt` forms are exactly
//! the ones the contributor had before the move (ADR 0027): only the
//! result changed shape — a [`UsageSnapshot`] instead of a finished display line, which the
//! `usage` footer component now formats.
//!
//! The key never reaches the journal, a log line or a notice: it is handed to the fetch and
//! nothing else. A failed refresh keeps the last snapshot and marks it stale (the runner's
//! job).

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, SystemTime};

use serde::Deserialize;
use serde_json::Value;

use crate::usage::{Remaining, UsageArgs, UsageFetcher, UsageSnapshot, UsageWindow};

/// The usage endpoint.
pub const USAGE_URL: &str = "https://opencode.ai/zen/go/v1/usage";

/// The refresh bounds (ADR 0027 C4).
pub const MIN_PERIOD: Duration = Duration::from_secs(15);
pub const MAX_PERIOD: Duration = Duration::from_secs(600);
pub const DEFAULT_PERIOD: Duration = Duration::from_secs(60);

/// Clamps a configured period into the allowed range.
pub fn clamp_period(seconds: u64) -> Duration {
    Duration::from_secs(seconds.clamp(MIN_PERIOD.as_secs(), MAX_PERIOD.as_secs()))
}

#[derive(Debug, Deserialize)]
struct UsageResponse {
    usage: Option<UsageWindows>,
}

#[derive(Debug, Deserialize, Default)]
struct UsageWindows {
    rolling: Option<Window>,
    weekly: Option<Window>,
    monthly: Option<Window>,
}

#[derive(Debug, Deserialize)]
struct Window {
    percent: Option<f64>,
    /// A string or a number (ADR 0027). Kept as a raw [`Value`] so one window's bad
    /// `resetsAt` drops only that window's countdown, never the whole response.
    #[serde(rename = "resetsAt")]
    resets_at: Option<Value>,
}

/// The remaining percentage for a window: `100 − percent` (the API reports **used**),
/// clamped to `[0, 100]`.
fn remaining(window: &Window) -> Option<f64> {
    window
        .percent
        .map(|percent| (100.0 - percent).clamp(0.0, 100.0))
}

/// One window's label (`rolling` → `5h`, `weekly` → `7d`, `monthly` → `30d`, pi's names).
fn label(name: &str) -> &'static str {
    match name {
        "rolling" => "5h",
        "weekly" => "7d",
        _ => "30d",
    }
}

/// The snapshot for a response (ADR 0027): one window per window the
/// API reports, each with its **remaining** share and its reset countdown. `None` when the
/// response carries no usable window at all — the row then keeps its last snapshot and goes
/// stale, exactly as the old `display` returning `None` did.
fn snapshot(response: &UsageResponse, now: SystemTime) -> Option<UsageSnapshot> {
    let usage = response.usage.as_ref()?;
    let windows = [
        ("rolling", usage.rolling.as_ref()),
        ("weekly", usage.weekly.as_ref()),
        ("monthly", usage.monthly.as_ref()),
    ];
    let mut out = Vec::new();
    for (name, window) in windows {
        let Some(window) = window else {
            continue;
        };
        let remaining = remaining(window).map(Remaining::Percent);
        let resets_at = window
            .resets_at
            .as_ref()
            .and_then(|at| countdown_value(at, now));
        if remaining.is_none() && resets_at.is_none() {
            continue;
        }
        out.push(UsageWindow {
            label: label(name).to_owned(),
            remaining,
            resets_at,
        });
    }
    if out.is_empty() {
        return None;
    }
    Some(UsageSnapshot {
        provider: PROVIDER.to_owned(),
        // "remaining" is explicit: the API's `percent` is used, this is `100 − percent`.
        title: "Usage".to_owned(),
        windows: out,
        plan: None,
    })
}

/// `↺4h19m`-style countdown from a `resetsAt` that is either an RFC 3339 string or an epoch
/// number (ADR 0027). `None` when it does not parse or is already past.
fn countdown_value(resets_at: &Value, now: SystemTime) -> Option<String> {
    match resets_at {
        Value::String(text) => countdown(text, now),
        Value::Number(number) => {
            let value = number.as_i64()?;
            // Epoch seconds (~1.7e9) vs milliseconds (~1.7e12): a magnitude test, so both the
            // upstream client's forms are accepted.
            let seconds = if value >= 100_000_000_000 {
                value / 1000
            } else {
                value
            };
            countdown_from_epoch(u64::try_from(seconds).ok()?, now)
        }
        _ => None,
    }
}

/// `↺4h19m`-style countdown from an RFC 3339 `resetsAt`. `None` when it does not parse or is
/// already past. The contributor's own clock is fine here (a background task, ADR 0023 U3).
fn countdown(resets_at: &str, now: SystemTime) -> Option<String> {
    countdown_from_epoch(parse_rfc3339_utc(resets_at)?, now)
}

/// The countdown text from a reset timestamp (Unix seconds).
fn countdown_from_epoch(reset: u64, now: SystemTime) -> Option<String> {
    let now = now.duration_since(SystemTime::UNIX_EPOCH).ok()?.as_secs();
    let secs = reset.checked_sub(now)?;
    Some(if secs >= 86_400 {
        format!("{}d{}h", secs / 86_400, (secs % 86_400) / 3_600)
    } else if secs >= 3_600 {
        format!("{}h{}m", secs / 3_600, (secs % 3_600) / 60)
    } else {
        format!("{}m", secs / 60)
    })
}

/// `YYYY-MM-DDTHH:MM:SS[.fff]Z` (UTC only, which is what the API sends) as a Unix timestamp.
/// Hand-rolled because the workspace has no date crate and the format is fixed.
fn parse_rfc3339_utc(text: &str) -> Option<u64> {
    if text.len() < 20 || !text.ends_with('Z') {
        return None;
    }
    let num = |range: std::ops::Range<usize>| -> Option<i64> { text.get(range)?.parse().ok() };
    let year = num(0..4)?;
    let month = num(5..7)?;
    let day = num(8..10)?;
    let hour = num(11..13)?;
    let minute = num(14..16)?;
    let second = num(17..19)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Days from civil (Howard Hinnant's algorithm), then the time of day.
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 + hour * 3_600 + minute * 60 + second).max(0) as u64)
}

/// One request's limit, well under the refresh period.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// An error and its sources, joined with `: `. reqwest's own message is only "error sending
/// request for url (…)"; the cause (DNS, connect, TLS, timeout) is in the source chain.
fn chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.contains(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

/// The provider id this source fetches for (the snapshot's `provider`).
const PROVIDER: &str = "opencode-go";

/// The opencode-go usage fetch (ADR 0027).
pub struct OpencodeGoUsage {
    key: String,
    url: String,
    client: reqwest::Client,
}

/// The descriptor's `Endpoint` build for opencode-go: the key the TUI resolved from `auth.json`
/// and the descriptor's endpoint. The period is the runner's, not the fetch's.
pub fn endpoint(args: &UsageArgs) -> Box<dyn UsageFetcher> {
    Box::new(OpencodeGoUsage::with_url(
        args.secret.clone().unwrap_or_default(),
        args.url.clone(),
    ))
}

impl OpencodeGoUsage {
    /// For tests: point the endpoint at a local mock.
    pub fn with_url(key: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            url: url.into(),
            // A refresh that hangs would hold the runner's task; the period is a minute.
            client: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }

    async fn fetch(&self) -> Result<UsageSnapshot, String> {
        let response = self
            .client
            .get(&self.url)
            .bearer_auth(&self.key)
            .send()
            .await
            .map_err(|error| format!("opencode-go usage request failed: {}", chain(&error)))?;
        if !response.status().is_success() {
            // Never echo the body: it is the provider's, and the key is a header, not a body.
            return Err(format!(
                "opencode-go usage returned HTTP {}",
                response.status().as_u16()
            ));
        }
        let body = response
            .text()
            .await
            .map_err(|error| format!("opencode-go usage body failed: {error}"))?;
        let parsed: UsageResponse = serde_json::from_str(&body)
            .map_err(|error| format!("opencode-go usage was not JSON: {error}"))?;
        snapshot(&parsed, SystemTime::now())
            .ok_or_else(|| "opencode-go usage had no windows".to_owned())
    }
}

impl UsageFetcher for OpencodeGoUsage {
    fn id(&self) -> &'static str {
        PROVIDER
    }

    fn fetch(&self) -> Pin<Box<dyn Future<Output = Result<UsageSnapshot, String>> + Send + '_>> {
        Box::pin(self.fetch())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> UsageResponse {
        serde_json::from_str(body).expect("valid JSON")
    }

    /// The API's `percent` is **used**; the snapshot carries the remaining `100 − percent`.
    #[test]
    fn the_snapshot_is_the_remaining_share() {
        let response = parse(
            r#"{"usage":{"rolling":{"status":"ok","percent":2,"resetsAt":"2030-01-01T00:00:00Z"},
                           "weekly":{"percent":19},"monthly":{"percent":9}}}"#,
        );
        let snapshot = snapshot(&response, SystemTime::UNIX_EPOCH).expect("a snapshot");
        assert_eq!(snapshot.provider, "opencode-go");
        assert_eq!(snapshot.title, "Usage");
        assert_eq!(snapshot.plan, None);
        assert_eq!(
            snapshot.windows,
            vec![
                UsageWindow {
                    label: "5h".to_owned(),
                    remaining: Some(Remaining::Percent(98.0)),
                    // 2030-01-01 is far from the epoch: days remain.
                    resets_at: Some("21915d0h".to_owned()),
                },
                UsageWindow {
                    label: "7d".to_owned(),
                    remaining: Some(Remaining::Percent(81.0)),
                    resets_at: None,
                },
                UsageWindow {
                    label: "30d".to_owned(),
                    remaining: Some(Remaining::Percent(91.0)),
                    resets_at: None,
                },
            ]
        );
    }

    /// `resetsAt` accepts epoch seconds and milliseconds (ADR 0027), and both give
    /// the snapshot the pre-move `display` drew as `5h ↺1h0m`.
    #[test]
    fn resets_at_accepts_epoch_numbers() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        let expected = vec![UsageWindow {
            label: "5h".to_owned(),
            remaining: Some(Remaining::Percent(98.0)),
            resets_at: Some("1h0m".to_owned()),
        }];
        let seconds = parse(r#"{"usage":{"rolling":{"percent":2,"resetsAt":1000003600}}}"#);
        assert_eq!(
            snapshot(&seconds, now).expect("a snapshot").windows,
            expected
        );
        let millis = parse(r#"{"usage":{"rolling":{"percent":2,"resetsAt":1000003600000}}}"#);
        assert_eq!(
            snapshot(&millis, now).expect("a snapshot").windows,
            expected
        );
    }

    /// One window's bad `resetsAt` drops only that window's countdown; its share and the other
    /// windows are unaffected (ADR 0027).
    #[test]
    fn one_bad_window_does_not_drop_the_row() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        let response = parse(
            r#"{"usage":{"rolling":{"percent":2,"resetsAt":{"bad":true}},
                         "weekly":{"percent":19,"resetsAt":1000003600},
                         "monthly":{"percent":9}}}"#,
        );
        let snapshot = snapshot(&response, now).expect("a snapshot");
        assert_eq!(
            snapshot.windows,
            vec![
                UsageWindow {
                    label: "5h".to_owned(),
                    remaining: Some(Remaining::Percent(98.0)),
                    // The bad countdown is dropped, the share stays.
                    resets_at: None,
                },
                UsageWindow {
                    label: "7d".to_owned(),
                    remaining: Some(Remaining::Percent(81.0)),
                    resets_at: Some("1h0m".to_owned()),
                },
                UsageWindow {
                    label: "30d".to_owned(),
                    remaining: Some(Remaining::Percent(91.0)),
                    resets_at: None,
                },
            ]
        );
    }

    /// No windows at all is no snapshot (the row disappears), and the period is clamped.
    #[test]
    fn no_windows_and_the_period_bounds() {
        assert!(snapshot(&parse(r#"{"usage":{}}"#), SystemTime::UNIX_EPOCH).is_none());
        assert!(snapshot(&parse(r#"{}"#), SystemTime::UNIX_EPOCH).is_none());
        assert_eq!(clamp_period(1), MIN_PERIOD);
        assert_eq!(clamp_period(60), Duration::from_secs(60));
        assert_eq!(clamp_period(100_000), MAX_PERIOD);
    }

    /// The fetch reads a local mock (never the real key or network), returns the remaining share
    /// as a snapshot, and never echoes the key.
    #[tokio::test]
    async fn the_fetch_reads_the_endpoint_into_a_snapshot() {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf);
            let body = r#"{"usage":{"rolling":{"percent":2},"weekly":{"percent":19}}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        });
        let fetcher = OpencodeGoUsage::with_url("sk-secret", format!("http://{addr}/usage"));
        let snapshot = fetcher.fetch().await.expect("a fetch");
        assert_eq!(snapshot.provider, "opencode-go");
        assert_eq!(
            snapshot.windows,
            vec![
                UsageWindow {
                    label: "5h".to_owned(),
                    remaining: Some(Remaining::Percent(98.0)),
                    resets_at: None,
                },
                UsageWindow {
                    label: "7d".to_owned(),
                    remaining: Some(Remaining::Percent(81.0)),
                    resets_at: None,
                },
            ]
        );
        let text = format!("{snapshot:?}");
        assert!(
            !text.contains("sk-secret"),
            "the key must not appear: {text}"
        );
    }

    /// An HTTP error never carries the key in its message.
    #[tokio::test]
    async fn an_http_error_does_not_leak_the_key() {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
        });
        let fetcher = OpencodeGoUsage::with_url("sk-secret", format!("http://{addr}/usage"));
        let error = fetcher.fetch().await.expect_err("an error");
        assert!(!error.contains("sk-secret"), "{error}");
    }

    /// A request that cannot connect names the cause, not only reqwest's "error sending
    /// request" (that line alone said nothing about why).
    #[tokio::test]
    async fn a_failed_request_names_its_cause() {
        let addr = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap()
        };
        let fetcher = OpencodeGoUsage::with_url("sk-secret", format!("http://{addr}/usage"));
        let error = fetcher.fetch().await.expect_err("nothing listens");
        assert!(error.contains("error sending request"), "{error}");
        assert!(error.to_lowercase().contains("refused"), "{error}");
        assert!(!error.contains("sk-secret"), "{error}");
    }

    /// A window with no `percent` contributes nothing, and a used 0 is 100% remaining.
    #[test]
    fn remaining_clamps() {
        let response =
            parse(r#"{"usage":{"rolling":{"percent":0},"weekly":{"percent":100},"monthly":{}}}"#);
        let snapshot = snapshot(&response, SystemTime::UNIX_EPOCH).unwrap();
        assert_eq!(
            snapshot.windows,
            vec![
                UsageWindow {
                    label: "5h".to_owned(),
                    remaining: Some(Remaining::Percent(100.0)),
                    resets_at: None,
                },
                UsageWindow {
                    label: "7d".to_owned(),
                    remaining: Some(Remaining::Percent(0.0)),
                    resets_at: None,
                },
            ],
            "no percent, no window"
        );
    }
}
