//! The `chatgpt` usage contributor (ADR 0026, ADR 0027 C4).
//!
//! `GET {base}/wham/usage` with the same auth headers a model request carries (the access
//! token, refreshed first if near expiry, and the account id decoded fresh — slice ①'s
//! `ensure_fresh`/`decode_account_id`), on the same clamp/period/"one failed window doesn't
//! drop the other" rules [`crate::opencode_go`] already established for C4. It runs only
//! while the current or a logged-in provider is `chatgpt` (the runner's gate, wired the same
//! way `OpencodeGoUsage`'s is). The response is never logged; only the display text enters
//! the contribution store.
//!
//! Response shape (`rate_limit.primary_window`/`secondary_window`, each
//! `used_percent`/`limit_window_seconds`/`reset_after_seconds`, and `plan_type`) verified
//! against `@oh-my-pi/pi-ai/src/usage/openai-codex.ts:21-46,61-91` (read-only reference,
//! 2026-10-16). Unlike opencode-go's `resetsAt` (a timestamp needing RFC 3339/epoch parsing),
//! `reset_after_seconds` is already a countdown, so no clock arithmetic is needed here.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::Deserialize;

use crate::contrib::{Contribution, Segment, SegmentRole, StatusContributor};
use crate::oauth_chatgpt::{self, Endpoints};

/// Everything before `/wham/usage`.
pub const USAGE_BASE: &str = "https://chatgpt.com/backend-api";

#[derive(Debug, Deserialize)]
struct UsageResponse {
    plan_type: Option<String>,
    rate_limit: Option<RateLimit>,
}

#[derive(Debug, Deserialize)]
struct RateLimit {
    primary_window: Option<Window>,
    secondary_window: Option<Window>,
}

#[derive(Debug, Deserialize)]
struct Window {
    used_percent: Option<f64>,
    limit_window_seconds: Option<u64>,
    reset_after_seconds: Option<u64>,
}

/// The remaining percentage for a window: `100 − used_percent`, clamped to `[0, 100]`.
fn remaining(window: &Window) -> Option<f64> {
    window
        .used_percent
        .map(|percent| (100.0 - percent).clamp(0.0, 100.0))
}

/// A window's length as a short label (`5h`, `7d`), derived from `limit_window_seconds`
/// itself rather than assumed from "primary"/"secondary" — the ADR's background does not say
/// which window is which duration, so this reads it from the response instead of guessing.
fn window_label(seconds: u64) -> String {
    if seconds >= 86_400 {
        format!("{}d", seconds / 86_400)
    } else if seconds >= 3_600 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}m", (seconds / 60).max(1))
    }
}

/// `↺4h19m`-style countdown from a countdown-in-seconds (already relative, unlike
/// opencode-go's absolute `resetsAt`).
fn countdown(seconds: u64) -> String {
    if seconds >= 86_400 {
        format!("{}d{}h", seconds / 86_400, (seconds % 86_400) / 3_600)
    } else if seconds >= 3_600 {
        format!("{}h{}m", seconds / 3_600, (seconds % 3_600) / 60)
    } else {
        format!("{}m", seconds / 60)
    }
}

/// The one-line display: the **remaining** share per window, then each window's reset
/// countdown, then the plan type. `None` when the response carries no window at all (ADR
/// 0027 C4: "one failed window dropped alone" — a window missing `used_percent` contributes
/// no share but the other window, if present, still shows).
fn display(response: &UsageResponse) -> Option<String> {
    let rate_limit = response.rate_limit.as_ref()?;
    let windows = [
        ("primary", rate_limit.primary_window.as_ref()),
        ("secondary", rate_limit.secondary_window.as_ref()),
    ];
    let mut shares = Vec::new();
    let mut resets = Vec::new();
    for (_name, window) in windows {
        let Some(window) = window else {
            continue;
        };
        let label = window
            .limit_window_seconds
            .map(window_label)
            .unwrap_or_else(|| "window".to_owned());
        if let Some(remaining) = remaining(window) {
            shares.push(format!("{label}: {remaining:.0}%"));
        }
        if let Some(seconds) = window.reset_after_seconds {
            resets.push(format!("{label} ↺{}", countdown(seconds)));
        }
    }
    if shares.is_empty() && resets.is_empty() {
        return None;
    }
    let mut out = String::from("ChatGPT usage (remaining): ");
    out.push_str(&shares.join(" · "));
    if !resets.is_empty() {
        out.push_str(" · ");
        out.push_str(&resets.join(" · "));
    }
    if let Some(plan) = &response.plan_type {
        out.push_str(&format!(" · {plan}"));
    }
    Some(out)
}

/// One request's limit, well under the refresh period.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// The `chatgpt` usage contributor.
pub struct ChatGptUsage {
    usage_url: String,
    session_id: String,
    period: Duration,
    endpoints: Endpoints,
    client: reqwest::Client,
}

impl ChatGptUsage {
    pub fn new(session_id: impl Into<String>, period: Duration) -> Self {
        Self::with_url(
            format!("{USAGE_BASE}/wham/usage"),
            session_id,
            period,
            Endpoints::default(),
        )
    }

    /// For tests: point the usage and the OAuth endpoints at local mocks.
    pub fn with_url(
        usage_url: impl Into<String>,
        session_id: impl Into<String>,
        period: Duration,
        endpoints: Endpoints,
    ) -> Self {
        Self {
            usage_url: usage_url.into(),
            session_id: session_id.into(),
            period,
            endpoints,
            client: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }

    async fn fetch(&self) -> Result<Vec<Contribution>, String> {
        let access = oauth_chatgpt::ensure_fresh(&self.endpoints)
            .await
            .map_err(|error| format!("chatgpt usage: {error}"))?;
        let account_id = oauth_chatgpt::decode_account_id(&access)
            .map_err(|error| format!("chatgpt usage: {error}"))?;
        let response = self
            .client
            .get(&self.usage_url)
            .bearer_auth(&access)
            .header("chatgpt-account-id", account_id)
            .header("OpenAI-Beta", "responses=experimental")
            .header("originator", oauth_chatgpt::ORIGINATOR)
            .header("session_id", &self.session_id)
            .send()
            .await
            .map_err(|error| format!("chatgpt usage request failed: {error}"))?;
        if !response.status().is_success() {
            // Never echo the body: it is the provider's, and the token is a header, not a
            // body — but an error body could still echo something sensitive back.
            return Err(format!(
                "chatgpt usage returned HTTP {}",
                response.status().as_u16()
            ));
        }
        let body = response
            .text()
            .await
            .map_err(|error| format!("chatgpt usage body failed: {error}"))?;
        let parsed: UsageResponse = serde_json::from_str(&body)
            .map_err(|error| format!("chatgpt usage was not JSON: {error}"))?;
        let text = display(&parsed).ok_or_else(|| "chatgpt usage had no windows".to_owned())?;
        Ok(vec![Contribution {
            item: "chatgpt.usage".to_owned(),
            segments: vec![Segment::new(text, SegmentRole::Normal)],
            stale: false,
        }])
    }
}

impl StatusContributor for ChatGptUsage {
    fn id(&self) -> &'static str {
        "chatgpt"
    }

    fn items(&self) -> &'static [&'static str] {
        &["chatgpt.usage"]
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

    /// `used_percent` is **used**; the display is the remaining `100 − used_percent`, and the
    /// window label is derived from `limit_window_seconds`, not assumed from the field name.
    #[test]
    fn the_display_is_the_remaining_share_with_a_derived_label() {
        let response = parse(
            r#"{"plan_type":"pro","rate_limit":{
                "primary_window":{"used_percent":5,"limit_window_seconds":18000,"reset_after_seconds":3600},
                "secondary_window":{"used_percent":40,"limit_window_seconds":604800,"reset_after_seconds":90000}
            }}"#,
        );
        let text = display(&response).expect("a display");
        assert!(text.contains("ChatGPT usage (remaining):"), "{text}");
        assert!(text.contains("5h: 95%"), "{text}");
        assert!(text.contains("7d: 60%"), "{text}");
        assert!(text.contains("5h ↺1h0m"), "{text}");
        assert!(text.contains("7d ↺1d1h"), "{text}");
        assert!(text.contains("· pro"), "{text}");
    }

    /// One window missing entirely does not drop the other (ADR 0027 C4).
    #[test]
    fn one_missing_window_does_not_drop_the_other() {
        let response = parse(
            r#"{"rate_limit":{
                "primary_window":{"used_percent":10,"limit_window_seconds":18000,"reset_after_seconds":100}
            }}"#,
        );
        let text = display(&response).expect("a display");
        assert!(text.contains("5h: 90%"), "{text}");
        assert!(!text.contains("secondary"), "{text}");
    }

    /// No windows at all (and no `rate_limit`) is no display.
    #[test]
    fn no_windows_is_no_display() {
        assert!(display(&parse(r#"{"rate_limit":{}}"#)).is_none());
        assert!(display(&parse(r#"{}"#)).is_none());
    }

    #[test]
    fn window_label_and_countdown_bucket_by_duration() {
        assert_eq!(window_label(1_800), "30m");
        assert_eq!(window_label(18_000), "5h");
        assert_eq!(window_label(604_800), "7d");
        assert_eq!(countdown(90), "1m");
        assert_eq!(countdown(3_661), "1h1m");
        assert_eq!(countdown(90_000), "1d1h");
    }
}
