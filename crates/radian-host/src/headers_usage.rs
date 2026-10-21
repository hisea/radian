//! Usage from response rate-limit headers (ADR 0027 slice ④).
//!
//! Providers without a usage endpoint (`anthropic`, `openai`) report their windows on every
//! model response instead: the adapter keeps the raw headers on
//! [`agent_core::ModelResponse::limits`], the turn hands them to the host on
//! [`agent_core::AgentEvent::ModelResponseReceived`], and this module parses them into the
//! same [`UsageSnapshot`] the footer `usage` component and `/usage` read for the endpoint
//! sources — same rendering rules, same one-line-per-window form.
//!
//! Only the header names the providers' docs name are parsed (listed on
//! [`ANTHROPIC_HEADERS`] / [`OPENAI_HEADERS`]); anything else stays raw on `limits` and is
//! ignored here, never invented ([`crates/agent-provider-contract/README.md`](../../agent-provider-contract/README.md):
//! a provider that lacks a concept is declared absent, never fabricated). A window whose
//! remaining fails to parse still shows while it has a reset, and vice versa (ADR 0027
//! revision 5: one bad field drops only its own part).

use std::time::SystemTime;

use agent_core::RateLimits;

use crate::usage::{Remaining, UsageSnapshot, UsageWindow};

/// One rate-limit window, named by the exact response header names the provider's docs give
/// for its remaining count and its reset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeadersWindow {
    /// The window's short label (`requests`, `tokens`).
    pub label: &'static str,
    /// The exact header carrying the remaining count (a plain integer).
    pub remaining: &'static str,
    /// The exact header carrying the reset (a duration, an epoch, or a timestamp — see
    /// [`reset_countdown`]).
    pub reset: &'static str,
}

/// A `Headers` usage source: the snapshot title and the windows its headers form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeadersUsage {
    /// The line's leading label (`Anthropic usage`, `OpenAI usage`).
    pub title: &'static str,
    /// The windows, in display order.
    pub windows: &'static [HeadersWindow],
}

/// The `anthropic` descriptor's `Headers` source. Names from the provider's rate-limit docs
/// (`crates/agent-provider-anthropic/WIRE.md` §9): `anthropic-ratelimit-requests-*` and
/// `anthropic-ratelimit-tokens-*`, each a limit, a remaining count and a reset.
pub const ANTHROPIC_HEADERS: HeadersUsage = HeadersUsage {
    title: "Anthropic usage",
    windows: &[
        HeadersWindow {
            label: "requests",
            remaining: "anthropic-ratelimit-requests-remaining",
            reset: "anthropic-ratelimit-requests-reset",
        },
        HeadersWindow {
            label: "tokens",
            remaining: "anthropic-ratelimit-tokens-remaining",
            reset: "anthropic-ratelimit-tokens-reset",
        },
    ],
};

/// The `openai` descriptor's `Headers` source. Names from the provider's rate-limit docs
/// (`crates/agent-provider-openai-compatible/WIRE.md`): `x-ratelimit-remaining-*` counts with
/// `x-ratelimit-reset-*` windows.
pub const OPENAI_HEADERS: HeadersUsage = HeadersUsage {
    title: "OpenAI usage",
    windows: &[
        HeadersWindow {
            label: "requests",
            remaining: "x-ratelimit-remaining-requests",
            reset: "x-ratelimit-reset-requests",
        },
        HeadersWindow {
            label: "tokens",
            remaining: "x-ratelimit-remaining-tokens",
            reset: "x-ratelimit-reset-tokens",
        },
    ],
};

/// Finds one header's value. Adapter names are already lower-case (reqwest's own form), and so
/// are the declared names; the match is exact, so a misread name matches nothing.
fn header<'a>(limits: &'a RateLimits, name: &str) -> Option<&'a str> {
    limits
        .headers
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

/// The snapshot for one response's headers, or `None` when no declared window yields a
/// remaining count or a reset — the row then has nothing to draw. `None` limits (the provider
/// sent no headers) is also `None`: never an invented snapshot.
pub fn snapshot(
    provider: &str,
    source: &HeadersUsage,
    limits: Option<&RateLimits>,
    now: SystemTime,
) -> Option<UsageSnapshot> {
    let limits = limits?;
    let mut out = Vec::new();
    for window in source.windows {
        let remaining = header(limits, window.remaining)
            .map(str::trim)
            .and_then(|v| v.parse::<u64>().ok())
            .map(Remaining::Count);
        let resets_at = header(limits, window.reset).and_then(|v| reset_countdown(v, now));
        if remaining.is_none() && resets_at.is_none() {
            continue;
        }
        out.push(UsageWindow {
            label: window.label.to_owned(),
            remaining,
            resets_at,
        });
    }
    if out.is_empty() {
        return None;
    }
    Some(UsageSnapshot {
        provider: provider.to_owned(),
        title: source.title.to_owned(),
        windows: out,
        plan: None,
    })
}

/// The snapshot for the current route's provider when its descriptor declares a `Headers`
/// source, else `None`. The one call the TUI's event path makes per model response.
pub fn headers_snapshot(provider: &str, limits: Option<&RateLimits>) -> Option<UsageSnapshot> {
    let source = match crate::provider::descriptor(provider)?.usage.as_ref()? {
        crate::provider::UsageSource::Headers(source) => source,
        crate::provider::UsageSource::Endpoint(_) => return None,
    };
    snapshot(provider, source, limits, SystemTime::now())
}

/// A reset header as the footer's countdown text. Three shapes, most specific first:
///
/// - a Go-style duration (`1s`, `6m0s`, `1h2m3s` — what the OpenAI headers carry);
/// - a bare integer: an epoch (seconds, or milliseconds past `100_000_000_000`) when large,
///   relative seconds when small;
/// - an RFC 3339 UTC timestamp (what the Anthropic headers carry).
///
/// `None` when nothing parses or the reset is already past: the window's share still shows.
fn reset_countdown(value: &str, now: SystemTime) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Some(seconds) = parse_duration(value) {
        return (seconds > 0).then(|| countdown(seconds));
    }
    if value.bytes().all(|b| b.is_ascii_digit()) {
        let number: u64 = value.parse().ok()?;
        if number >= 100_000_000_000 {
            return countdown_from_epoch(number / 1000, now);
        }
        if number >= 1_000_000_000 {
            return countdown_from_epoch(number, now);
        }
        return (number > 0).then(|| countdown(number));
    }
    countdown_from_epoch(parse_rfc3339_utc(value)?, now)
}

/// A Go-style duration (`1h2m3s`, `6m0s`, `90s`) as seconds. Only `h`/`m`/`s` units, at least
/// one pair, nothing else: anything else is not a duration we understand.
fn parse_duration(value: &str) -> Option<u64> {
    let mut seconds = 0u64;
    let mut number: u64 = 0;
    let mut has_number = false;
    let mut has_pair = false;
    for byte in value.bytes() {
        if byte.is_ascii_digit() {
            number = number.checked_mul(10)?.checked_add((byte - b'0') as u64)?;
            has_number = true;
        } else if has_number && matches!(byte, b'h' | b'm' | b's') {
            let unit = match byte {
                b'h' => 3_600,
                b'm' => 60,
                _ => 1,
            };
            seconds = seconds.checked_add(number.checked_mul(unit)?)?;
            number = 0;
            has_number = false;
            has_pair = true;
        } else {
            return None;
        }
    }
    (has_pair && !has_number).then_some(seconds)
}

/// `↺4h19m`-style countdown from a relative duration in seconds.
fn countdown(seconds: u64) -> String {
    if seconds >= 86_400 {
        format!("{}d{}h", seconds / 86_400, (seconds % 86_400) / 3_600)
    } else if seconds >= 3_600 {
        format!("{}h{}m", seconds / 3_600, (seconds % 3_600) / 60)
    } else {
        format!("{}m", (seconds / 60).max(1))
    }
}

/// `↺4h19m`-style countdown from a reset timestamp (Unix seconds). `None` when already past.
fn countdown_from_epoch(reset: u64, now: SystemTime) -> Option<String> {
    let now = now.duration_since(SystemTime::UNIX_EPOCH).ok()?.as_secs();
    let secs = reset.checked_sub(now)?;
    (secs > 0).then(|| countdown(secs))
}

/// `YYYY-MM-DDTHH:MM:SS[.fff]Z` (UTC only) as a Unix timestamp. Hand-rolled: the workspace has
/// no date crate and the format is fixed (the same shape `opencode_go.rs` reads).
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
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 + hour * 3_600 + minute * 60 + second).max(0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(pairs: &[(&str, &str)]) -> RateLimits {
        RateLimits {
            headers: pairs
                .iter()
                .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
                .collect(),
        }
    }

    /// Every documented header parses: two windows, counts and countdowns.
    #[test]
    fn the_anthropic_headers_form_two_windows() {
        let now = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        let received = limits(&[
            ("anthropic-ratelimit-requests-limit", "1000"),
            ("anthropic-ratelimit-requests-remaining", "999"),
            ("anthropic-ratelimit-requests-reset", "2001-09-09T02:46:40Z"),
            ("anthropic-ratelimit-tokens-limit", "100000"),
            ("anthropic-ratelimit-tokens-remaining", "99900"),
            ("anthropic-ratelimit-tokens-reset", "2001-09-09T02:46:40Z"),
        ]);
        let snapshot =
            snapshot("anthropic", &ANTHROPIC_HEADERS, Some(&received), now).expect("a snapshot");
        assert_eq!(snapshot.provider, "anthropic");
        assert_eq!(snapshot.title, "Anthropic usage");
        assert_eq!(snapshot.plan, None);
        assert_eq!(
            snapshot.windows,
            vec![
                UsageWindow {
                    label: "requests".to_owned(),
                    remaining: Some(Remaining::Count(999)),
                    resets_at: Some("1h0m".to_owned()),
                },
                UsageWindow {
                    label: "tokens".to_owned(),
                    remaining: Some(Remaining::Count(99900)),
                    resets_at: Some("1h0m".to_owned()),
                },
            ]
        );
    }

    /// The OpenAI shape: remaining counts with duration resets.
    #[test]
    fn the_openai_headers_form_two_windows() {
        let now = SystemTime::UNIX_EPOCH;
        let received = limits(&[
            ("x-ratelimit-limit-requests", "10000"),
            ("x-ratelimit-remaining-requests", "9999"),
            ("x-ratelimit-reset-requests", "1s"),
            ("x-ratelimit-limit-tokens", "200000"),
            ("x-ratelimit-remaining-tokens", "199900"),
            ("x-ratelimit-reset-tokens", "6m0s"),
        ]);
        let snapshot =
            snapshot("openai", &OPENAI_HEADERS, Some(&received), now).expect("a snapshot");
        assert_eq!(snapshot.title, "OpenAI usage");
        assert_eq!(
            snapshot.windows,
            vec![
                UsageWindow {
                    label: "requests".to_owned(),
                    remaining: Some(Remaining::Count(9999)),
                    resets_at: Some("1m".to_owned()),
                },
                UsageWindow {
                    label: "tokens".to_owned(),
                    remaining: Some(Remaining::Count(199900)),
                    resets_at: Some("6m".to_owned()),
                },
            ]
        );
    }

    /// No headers is no snapshot — never an invented one.
    #[test]
    fn no_headers_is_no_snapshot() {
        let now = SystemTime::UNIX_EPOCH;
        assert!(snapshot("anthropic", &ANTHROPIC_HEADERS, None, now).is_none());
        assert!(snapshot("anthropic", &ANTHROPIC_HEADERS, Some(&limits(&[])), now).is_none());
        assert!(
            snapshot(
                "anthropic",
                &ANTHROPIC_HEADERS,
                Some(&limits(&[("x-unrelated", "1")])),
                now
            )
            .is_none()
        );
    }

    /// One bad field drops only its own part (ADR 0027): a window with an
    /// unreadable remaining still shows its reset, and vice versa.
    #[test]
    fn one_bad_field_drops_only_its_own_part() {
        let now = SystemTime::UNIX_EPOCH;
        let received = limits(&[
            ("anthropic-ratelimit-requests-remaining", "plenty"),
            ("anthropic-ratelimit-requests-reset", "60"),
            ("anthropic-ratelimit-tokens-remaining", "50"),
            ("anthropic-ratelimit-tokens-reset", "soon"),
        ]);
        let snapshot =
            snapshot("anthropic", &ANTHROPIC_HEADERS, Some(&received), now).expect("a snapshot");
        assert_eq!(
            snapshot.windows,
            vec![
                UsageWindow {
                    label: "requests".to_owned(),
                    remaining: None,
                    resets_at: Some("1m".to_owned()),
                },
                UsageWindow {
                    label: "tokens".to_owned(),
                    remaining: Some(Remaining::Count(50)),
                    resets_at: None,
                },
            ]
        );
    }

    /// A non-`Headers` provider never yields a snapshot here, whatever the headers say.
    #[test]
    fn an_endpoint_provider_has_no_headers_snapshot() {
        let received = limits(&[("x-ratelimit-remaining-requests", "5")]);
        assert!(headers_snapshot("opencode-go", Some(&received)).is_none());
        assert!(headers_snapshot("no-such-provider", Some(&received)).is_none());
    }
}
