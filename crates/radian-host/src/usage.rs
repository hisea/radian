//! Provider usage snapshots (ADR 0027).
//!
//! `usage` is a **provider-related** footer component: its data is one [`UsageSnapshot`] — the
//! provider it belongs to, the line's leading label, and the windows, each with its remaining
//! amount and reset countdown — and it reads nothing else. Where the snapshot comes from is the
//! provider descriptor's optional [`crate::provider::UsageSource`]; today the only variant is
//! `Endpoint`, which polls a URL and parses it (the shape `opencode_go.rs` and
//! `chatgpt_usage.rs` already had for ADR 0027 C4). A descriptor with no `usage` source draws
//! no row.
//!
//! The producers run off the draw path exactly like every other contributor (ADR 0027 C3):
//! each owns a tokio task, writes the shared [`ContributionStore`] and wakes the UI with one
//! dirty notification. A failed refresh keeps the last snapshot and marks it stale; a
//! descriptor's endpoint only yields when the current route's provider is its own (the caller's
//! gate, `spawn_gated`).
//!
//! The parsing and the clock stay **here**: a snapshot carries the reset countdown already
//! formatted, so the draw path needs no clock and no I/O (ADR 0023 U2/U3), and an absolute
//! `resetsAt` and a relative `reset_after_seconds` both arrive as the same field.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use crate::contrib::{Contribution, ContributionStore, StatusContributor, Tick};

/// The item id the `usage` component and every usage producer use (ADR 0027
/// decision 3).
pub const USAGE_ITEM: &str = "usage";

/// A window's remaining amount (ADR 0027: "a fraction or a count").
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Remaining {
    /// A share of the window, in percent (0–100). Both built-in sources publish one: their APIs
    /// carry a **used** percent, and the source subtracts it (`100 − used`), so the display can
    /// say "remaining" and mean it (ADR 0027 C4).
    Percent(f64),
    /// An absolute number of units left.
    Count(u64),
}

impl Remaining {
    /// The amount as the displays write it: a whole percent, or a plain count.
    pub fn text(&self) -> String {
        match self {
            Self::Percent(percent) => format!("{percent:.0}%"),
            Self::Count(count) => count.to_string(),
        }
    }
}

/// One window of a [`UsageSnapshot`].
#[derive(Debug, Clone, PartialEq)]
pub struct UsageWindow {
    /// The window's short label (`5h`, `7d`, `30d`), from the provider's own window name or its
    /// length.
    pub label: String,
    /// The remaining amount, when the provider reports one for this window. A window whose share
    /// is unreadable is still shown while it has a reset: one bad field drops only its own part
    /// (ADR 0027).
    pub remaining: Option<Remaining>,
    /// The reset countdown, already formatted (`4h19m`): the source normalizes an absolute
    /// `resetsAt` (RFC 3339 or epoch) and a relative `reset_after_seconds` into it, so the draw
    /// path computes no time. `None` when the provider's reset did not parse or is past.
    pub resets_at: Option<String>,
}

/// One provider's usage at one refresh (ADR 0027): the one structure the
/// `usage` footer component and the `/usage` command read.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageSnapshot {
    /// The provider id the snapshot belongs to (its descriptor id). The component draws only
    /// while this is the current route's provider.
    pub provider: String,
    /// The line's leading label, from the source (`Usage`, `ChatGPT usage`). Every display
    /// renders `{title} (remaining): …`: the number is the remaining share, and it says so.
    pub title: String,
    /// The windows, in the provider's own order.
    pub windows: Vec<UsageWindow>,
    /// A trailing word the provider adds after the windows (chatgpt's plan type), when it has
    /// one.
    pub plan: Option<String>,
}

/// One provider's usage fetch (ADR 0027): the descriptor's `Endpoint`
/// source builds it, the runner awaits it. A fetch is the only thing the descriptor has to
/// declare about *how* a provider's usage is obtained.
pub trait UsageFetcher: Send + Sync {
    /// The fetch's stable id: the provider id it fetches for. It is the snapshot's `provider`
    /// and the name a run of failures is noticed under.
    fn id(&self) -> &'static str;

    /// One refresh: the provider's request, then its answer parsed into a snapshot. `Err` keeps
    /// the last snapshot and marks it stale (the runner's job).
    fn fetch(&self) -> Pin<Box<dyn Future<Output = Result<UsageSnapshot, String>> + Send + '_>>;
}

/// The facts one `Endpoint` fetch is built from (ADR 0027): the
/// endpoint's URL (the descriptor's, so there is one source for it) and the session's own
/// facts. The secret stays in the process: it is handed to a fetch, never to a journal, a log
/// line or the store (ADR 0027 C4).
pub struct UsageArgs {
    /// The URL to poll.
    pub url: String,
    /// The routed session id (the chatgpt fetch's `session_id` header).
    pub session_id: String,
    /// The provider's resolved secret, when its spawn gate found one (the api key for
    /// `opencode-go`); `None` for a provider whose fetch refreshes its own login (chatgpt).
    pub secret: Option<String>,
}

/// A [`UsageFetcher`] as an ordinary contributor (ADR 0027 C3): its snapshot enters the store
/// as the [`USAGE_ITEM`] contribution, so the route gate, the stale marking and the
/// one-notice-per-run-of-failures are the shared contributor machinery, not a second path.
struct SnapshotContributor {
    fetcher: Box<dyn UsageFetcher>,
    period: Duration,
}

impl StatusContributor for SnapshotContributor {
    fn id(&self) -> &'static str {
        self.fetcher.id()
    }

    fn items(&self) -> Vec<String> {
        vec![USAGE_ITEM.to_owned()]
    }

    fn period(&self) -> Duration {
        self.period
    }

    fn refresh(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Contribution>, String>> + Send + '_>> {
        Box::pin(async move {
            let snapshot = self.fetcher.fetch().await?;
            Ok(vec![Contribution {
                item: USAGE_ITEM.to_owned(),
                // The snapshot **is** the value; the text segments stay empty, and the `usage`
                // component is the only thing that formats it.
                segments: Vec::new(),
                stale: false,
                symbol: None,
                usage: Some(snapshot),
            }])
        })
    }
}

/// Runs one provider's usage refresh, gated on the current route (ADR 0027): while
/// `active` is false the row hides and nothing is fetched (no network); the moment it becomes
/// true the source refreshes once, then every `period`.
pub fn spawn_gated(
    handle: &tokio::runtime::Handle,
    fetcher: Box<dyn UsageFetcher>,
    period: Duration,
    store: Arc<ContributionStore>,
    on_tick: Arc<dyn Fn(Tick) + Send + Sync>,
    active: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    crate::contrib::spawn_gated(
        handle,
        Arc::new(SnapshotContributor { fetcher, period }),
        store,
        on_tick,
        active,
    )
}
