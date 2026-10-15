//! The opencode-go usage contributor (ADR 0027 C4).
//!
//! `GET https://opencode.ai/zen/go/v1/usage` with the key from `auth.json` (ADR 0026 D2),
//! every 60 s by default (15 s–10 min, or off). It runs only when the current or a logged-in
//! provider is opencode-go. The API reports **used** percentages; the display shows the
//! **remaining** `100 − percent`, labeled, so the number cannot be misread (ADR 0027 C4).
//!
//! The key never reaches the journal, a log line or a notice: only the display text goes into
//! the contribution store. A failed refresh keeps the last value and marks it stale (the
//! runner's job).

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, SystemTime};

use serde::Deserialize;
use serde_json::Value;

use crate::contrib::{Contribution, Segment, SegmentRole, StatusContributor};

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

/// The one-line display: the **remaining** share per window, then each window's reset
/// countdown. `None` when the response carries no window at all.
fn display(response: &UsageResponse, now: SystemTime) -> Option<String> {
    let usage = response.usage.as_ref()?;
    let windows = [
        ("rolling", usage.rolling.as_ref()),
        ("weekly", usage.weekly.as_ref()),
        ("monthly", usage.monthly.as_ref()),
    ];
    let mut shares = Vec::new();
    let mut resets = Vec::new();
    for (name, window) in windows {
        let Some(window) = window else {
            continue;
        };
        if let Some(remaining) = remaining(window) {
            shares.push(format!("{}: {remaining:.0}%", label(name)));
        }
        if let Some(reset) = window
            .resets_at
            .as_ref()
            .and_then(|at| countdown_value(at, now))
        {
            resets.push(format!("{} ↺{reset}", label(name)));
        }
    }
    if shares.is_empty() && resets.is_empty() {
        return None;
    }
    // "remaining" is explicit: the API's `percent` is used, this is `100 − percent`.
    let mut out = String::from("Usage (remaining): ");
    out.push_str(&shares.join(" · "));
    if !resets.is_empty() {
        out.push_str(" · ");
        out.push_str(&resets.join(" · "));
    }
    Some(out)
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

/// The opencode-go usage contributor.
pub struct OpencodeGoUsage {
    key: String,
    url: String,
    period: Duration,
    client: reqwest::Client,
}

impl OpencodeGoUsage {
    pub fn new(key: impl Into<String>, period: Duration) -> Self {
        Self::with_url(key, USAGE_URL, period)
    }

    /// For tests: point the endpoint at a local mock.
    pub fn with_url(key: impl Into<String>, url: impl Into<String>, period: Duration) -> Self {
        Self {
            key: key.into(),
            url: url.into(),
            period,
            client: reqwest::Client::new(),
        }
    }

    async fn fetch(&self) -> Result<Vec<Contribution>, String> {
        let response = self
            .client
            .get(&self.url)
            .bearer_auth(&self.key)
            .send()
            .await
            .map_err(|error| format!("opencode-go usage request failed: {error}"))?;
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
        let text = display(&parsed, SystemTime::now())
            .ok_or_else(|| "opencode-go usage had no windows".to_owned())?;
        Ok(vec![Contribution {
            item: "opencode-go.usage".to_owned(),
            segments: vec![Segment::new(text, SegmentRole::Normal)],
            stale: false,
        }])
    }
}

impl StatusContributor for OpencodeGoUsage {
    fn id(&self) -> &'static str {
        "opencode-go"
    }

    fn items(&self) -> &'static [&'static str] {
        &["opencode-go.usage"]
    }

    fn period(&self) -> Duration {
        self.period
    }

    fn refresh(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Contribution>, String>> + Send + '_>> {
        Box::pin(self.fetch())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> UsageResponse {
        serde_json::from_str(body).expect("valid JSON")
    }

    /// The API's `percent` is **used**; the display is the remaining `100 − percent`.
    #[test]
    fn the_display_is_the_remaining_share() {
        let response = parse(
            r#"{"usage":{"rolling":{"status":"ok","percent":2,"resetsAt":"2030-01-01T00:00:00Z"},
                           "weekly":{"percent":19},"monthly":{"percent":9}}}"#,
        );
        let text = display(&response, SystemTime::UNIX_EPOCH).expect("a display");
        assert!(text.contains("Usage (remaining):"), "{text}");
        assert!(text.contains("5h: 98%"), "{text}");
        assert!(text.contains("7d: 81%"), "{text}");
        assert!(text.contains("30d: 91%"), "{text}");
        assert!(text.contains("↺"), "the reset countdown is shown: {text}");
        // 2030-01-01 is far from the epoch: days remain.
        assert!(text.contains("5h ↺"), "{text}");
    }

    /// `resetsAt` accepts epoch seconds and milliseconds (ADR 0027).
    #[test]
    fn resets_at_accepts_epoch_numbers() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        let seconds = parse(r#"{"usage":{"rolling":{"percent":2,"resetsAt":1000003600}}}"#);
        let text = display(&seconds, now).expect("a display");
        assert!(text.contains("5h ↺1h0m"), "{text}");
        let millis = parse(r#"{"usage":{"rolling":{"percent":2,"resetsAt":1000003600000}}}"#);
        let text = display(&millis, now).expect("a display");
        assert!(text.contains("5h ↺1h0m"), "{text}");
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
        let text = display(&response, now).expect("a display");
        assert!(
            text.contains("5h: 98%"),
            "the bad window's share stays: {text}"
        );
        assert!(text.contains("7d: 81%"), "{text}");
        assert!(text.contains("30d: 91%"), "{text}");
        assert!(text.contains("7d ↺1h0m"), "{text}");
        assert!(
            !text.contains("5h ↺"),
            "the bad countdown is dropped: {text}"
        );
    }

    /// No windows at all is no display (the row disappears), and the period is clamped.
    #[test]
    fn no_windows_and_the_period_bounds() {
        assert!(display(&parse(r#"{"usage":{}}"#), SystemTime::UNIX_EPOCH).is_none());
        assert!(display(&parse(r#"{}"#), SystemTime::UNIX_EPOCH).is_none());
        assert_eq!(clamp_period(1), MIN_PERIOD);
        assert_eq!(clamp_period(60), Duration::from_secs(60));
        assert_eq!(clamp_period(100_000), MAX_PERIOD);
    }

    /// The contributor fetches from a local mock (never the real key or network), publishes the
    /// remaining share, and never echoes the key.
    #[tokio::test]
    async fn the_contributor_fetches_and_publishes() {
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
        let contributor =
            OpencodeGoUsage::with_url("sk-secret", format!("http://{addr}/usage"), DEFAULT_PERIOD);
        let contributions = contributor.refresh().await.expect("a refresh");
        assert_eq!(contributions[0].item, "opencode-go.usage");
        let text: String = contributions[0]
            .segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect();
        assert!(text.contains("5h: 98%"), "{text}");
        assert!(text.contains("7d: 81%"), "{text}");
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
        let contributor =
            OpencodeGoUsage::with_url("sk-secret", format!("http://{addr}/usage"), DEFAULT_PERIOD);
        let error = contributor.refresh().await.expect_err("an error");
        assert!(!error.contains("sk-secret"), "{error}");
    }

    /// A window with no `percent` contributes nothing, and a used 0 is 100% remaining.
    #[test]
    fn remaining_clamps() {
        let response =
            parse(r#"{"usage":{"rolling":{"percent":0},"weekly":{"percent":100},"monthly":{}}}"#);
        let text = display(&response, SystemTime::UNIX_EPOCH).unwrap();
        assert!(text.contains("5h: 100%"), "{text}");
        assert!(text.contains("7d: 0%"), "{text}");
        assert!(!text.contains("30d"), "no percent, no window: {text}");
    }
}
