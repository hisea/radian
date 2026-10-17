//! The `chatgpt` usage source (ADR 0026, ADR 0027 C4; the descriptor's
//! `usage = Endpoint`).
//!
//! `GET {base}/wham/usage` with the same auth headers a model request carries (the access
//! token, refreshed first if near expiry, and the account id decoded fresh — slice ①'s
//! `ensure_fresh`/`decode_account_id`), on the same clamp/"one failed window doesn't drop the
//! other" rules [`crate::opencode_go`] already established for C4. It runs only while the
//! current or a logged-in provider is `chatgpt` (the spawn gate, wired the same way
//! `OpencodeGoUsage`'s is). The response is never logged; only the snapshot enters the
//! contribution store.
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

use crate::oauth_chatgpt::{self, Endpoints};
use crate::usage::{Remaining, UsageArgs, UsageFetcher, UsageSnapshot, UsageWindow};

/// Everything before `/wham/usage`.
pub const USAGE_BASE: &str = "https://chatgpt.com/backend-api";

/// The usage endpoint.
pub const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

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

/// The snapshot for a response (ADR 0027): one window per reported
/// window, each with its **remaining** share and its reset countdown, plus the plan type as the
/// snapshot's trailing word. `None` when the response carries no window at all (ADR 0027 C4:
/// "one failed window dropped alone" — a window missing `used_percent` contributes no share but
/// the other window, if present, still shows).
fn snapshot(response: &UsageResponse) -> Option<UsageSnapshot> {
    let rate_limit = response.rate_limit.as_ref()?;
    let windows = [
        ("primary", rate_limit.primary_window.as_ref()),
        ("secondary", rate_limit.secondary_window.as_ref()),
    ];
    let mut out = Vec::new();
    for (_name, window) in windows {
        let Some(window) = window else {
            continue;
        };
        let label = window
            .limit_window_seconds
            .map(window_label)
            .unwrap_or_else(|| "window".to_owned());
        let remaining = remaining(window).map(Remaining::Percent);
        let resets_at = window.reset_after_seconds.map(countdown);
        if remaining.is_none() && resets_at.is_none() {
            continue;
        }
        out.push(UsageWindow {
            label,
            remaining,
            resets_at,
        });
    }
    if out.is_empty() {
        return None;
    }
    Some(UsageSnapshot {
        provider: PROVIDER.to_owned(),
        title: "ChatGPT usage".to_owned(),
        windows: out,
        plan: response.plan_type.clone(),
    })
}

/// One request's limit, well under the refresh period.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// The provider id this source fetches for (the snapshot's `provider`).
const PROVIDER: &str = "chatgpt";

/// The `chatgpt` usage fetch (ADR 0027).
pub struct ChatGptUsage {
    usage_url: String,
    session_id: String,
    endpoints: Endpoints,
    client: reqwest::Client,
}

/// The descriptor's `Endpoint` build for `chatgpt`: the descriptor's endpoint and the session id
/// the headers carry. There is no stored secret to hand over — the fetch renews the oauth entry
/// itself, per call.
pub fn endpoint(args: &UsageArgs) -> Box<dyn UsageFetcher> {
    Box::new(ChatGptUsage::with_url(
        args.url.clone(),
        args.session_id.clone(),
        Endpoints::default(),
    ))
}

impl ChatGptUsage {
    /// For tests: point the usage and the OAuth endpoints at local mocks.
    pub fn with_url(
        usage_url: impl Into<String>,
        session_id: impl Into<String>,
        endpoints: Endpoints,
    ) -> Self {
        Self {
            usage_url: usage_url.into(),
            session_id: session_id.into(),
            endpoints,
            client: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }

    async fn fetch(&self) -> Result<UsageSnapshot, String> {
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
        snapshot(&parsed).ok_or_else(|| "chatgpt usage had no windows".to_owned())
    }
}

impl UsageFetcher for ChatGptUsage {
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

    /// `used_percent` is **used**; the snapshot carries the remaining `100 − used_percent`, and
    /// the window label is derived from `limit_window_seconds`, not assumed from the field name.
    #[test]
    fn the_snapshot_is_the_remaining_share_with_a_derived_label() {
        let response = parse(
            r#"{"plan_type":"pro","rate_limit":{
                "primary_window":{"used_percent":5,"limit_window_seconds":18000,"reset_after_seconds":3600},
                "secondary_window":{"used_percent":40,"limit_window_seconds":604800,"reset_after_seconds":90000}
            }}"#,
        );
        let snapshot = snapshot(&response).expect("a snapshot");
        assert_eq!(snapshot.provider, "chatgpt");
        assert_eq!(snapshot.title, "ChatGPT usage");
        assert_eq!(snapshot.plan.as_deref(), Some("pro"));
        assert_eq!(
            snapshot.windows,
            vec![
                UsageWindow {
                    label: "5h".to_owned(),
                    remaining: Some(Remaining::Percent(95.0)),
                    resets_at: Some("1h0m".to_owned()),
                },
                UsageWindow {
                    label: "7d".to_owned(),
                    remaining: Some(Remaining::Percent(60.0)),
                    resets_at: Some("1d1h".to_owned()),
                },
            ]
        );
    }

    /// One window missing entirely does not drop the other (ADR 0027 C4).
    #[test]
    fn one_missing_window_does_not_drop_the_other() {
        let response = parse(
            r#"{"rate_limit":{
                "primary_window":{"used_percent":10,"limit_window_seconds":18000,"reset_after_seconds":100}
            }}"#,
        );
        let snapshot = snapshot(&response).expect("a snapshot");
        assert_eq!(
            snapshot.windows,
            vec![UsageWindow {
                label: "5h".to_owned(),
                remaining: Some(Remaining::Percent(90.0)),
                resets_at: Some("1m".to_owned()),
            }]
        );
        assert!(snapshot.plan.is_none(), "no plan type, no trailing word");
    }

    /// No windows at all (and no `rate_limit`) is no snapshot.
    #[test]
    fn no_windows_is_no_snapshot() {
        assert!(snapshot(&parse(r#"{"rate_limit":{}}"#)).is_none());
        assert!(snapshot(&parse(r#"{}"#)).is_none());
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
