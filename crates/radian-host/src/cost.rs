//! The session's cost: what one model call cost, and what the session has cost so far.
//!
//! **Per call, at the price in force then** (ADR 0027, ADR 0007). The
//! fee is not "the session's token totals times the current model's price": a session can
//! switch models (`/model`, ADR 0026 D5), and a token total that mixes models cannot be
//! multiplied by one price. Instead each call is priced as it arrives, at the price the route
//! had at that moment, and the projection accumulates the results.
//!
//! The cached tokens are billed **once, at the cache rate**. `ModelUsage::uncached_input()`
//! is the adapter-declared answer to "how much of the input is not a cache hit?", which is
//! exactly what the full input rate applies to (ADR 0007: OpenAI's `prompt_tokens`
//! contains the hit, Anthropic's `input_tokens` does not — reading `input` directly billed the
//! hit at full price *and* at the cache rate, up to 10x too much on a cache-heavy session).

use std::sync::Arc;

use agent_core::ModelUsage;
use radian_runtime::ModelRoute;
use radian_session::{EventData, Session};

use crate::ModelPrice;
use crate::projection::SessionProjection;

/// Looks up a route's catalog prices from the **warm local cache only** (ADR 0026 D7: a switch
/// never fetches, and a cache miss means "no price"). Shared by the live path (the TUI resolves
/// it on startup and on every applied route change) and by [`restore_usage`].
pub type RoutePrice = Arc<dyn Fn(&str, &str) -> Option<ModelPrice> + Send + Sync>;

/// One call's cost in micro-dollars: each counter times its own rate, per million tokens.
///
/// `None` when the call has nothing to bill — no counter is non-zero, or no rate is known for
/// a counter that is. The item hides rather than showing `$0.00` for a call that cost nothing
/// visible (ADR 0027: a model with no price, an all-zero usage report, or a
/// response that only moved cached tokens whose rate the catalog does not list).
///
/// Each term is `tokens * rate / 1_000_000` truncated (so at most one micro-dollar is lost per
/// term, never gained) and the terms are added with saturation: a wildly wrong price cannot
/// wrap the total around.
pub fn cost_micros(price: ModelPrice, usage: &ModelUsage) -> Option<u64> {
    let mut total: u64 = 0;
    let mut billed = false;
    let mut add = |tokens: Option<u64>, rate: Option<u64>| {
        if let (Some(tokens), Some(rate)) = (tokens, rate) {
            total = total.saturating_add(tokens.saturating_mul(rate) / 1_000_000);
            // A counter the provider reported as 0 is not a billed term: an all-zero usage
            // report must not turn into `$0.00` (ADR 0027).
            billed |= tokens > 0;
        }
    };
    add(usage.uncached_input(), Some(price.input));
    add(usage.output, Some(price.output));
    add(usage.cache_read, price.cache_read);
    add(usage.cache_write, price.cache_write);
    billed.then_some(total)
}

/// Adds one call's fee to a session total. The running total stays `None` until the first
/// call that had something to bill, which is what keeps `$0.00` off the footer.
pub fn add_cost(total: Option<u64>, price: Option<ModelPrice>, usage: &ModelUsage) -> Option<u64> {
    let fee = price.and_then(|price| cost_micros(price, usage));
    match (total, fee) {
        (Some(total), Some(fee)) => Some(total.saturating_add(fee)),
        (Some(total), None) => Some(total),
        (None, Some(fee)) => Some(fee),
        (None, None) => None,
    }
}

/// Folds a resumed session's journal back into the projection's usage totals and fee
/// (F3, ADR 0027).
///
/// The TUI's projection starts empty, and its only writer is the live event sink — so before
/// this, `--resume` showed `tokens` as `-` and hid `cost` until the *next* response arrived,
/// losing everything the session had already spent (the number the owner most wants to see).
///
/// Both usage records count: `ModelUsageRecorded` (a conversation request) **and**
/// `CompactionUsageRecorded` — the summarizer's calls are real money (ADR 0018), and
/// the projection counts them live through `AgentEvent::CompactionUsage` (revision 7), so a
/// resume must not disagree with the run that produced the file.
///
/// **Each call is priced at the route in force at that point**, like the live path: the session
/// snapshot's route prices the calls before the first model `ConfigChanged`, and each such
/// change re-prices the ones after it (ADR 0026 D5). `price_for` resolves a route from the warm
/// local catalog only; a route it cannot price contributes no fee (never an invented number),
/// exactly as a live call under an unpriced route would.
///
/// Returns the number of usage records folded, so a caller can tell "nothing to restore" from
/// "restored nothing".
pub fn restore_usage(
    session: &Session,
    projection: &SessionProjection,
    price_for: &RoutePrice,
) -> usize {
    let mut price =
        route_of(&session_snapshot(session)).and_then(|route| price_for(&route.0, &route.1));
    let mut folded = 0;
    for event in session.events() {
        match &event.data {
            // ADR 0026 D5/D8: the applied route change moves the price for the calls after it.
            EventData::ConfigChanged { change, .. } => {
                if let Some(route) = ModelRoute::from_value(change) {
                    price = price_for(&route.provider, &route.model);
                }
            }
            EventData::ModelUsageRecorded { usage }
            | EventData::CompactionUsageRecorded { usage, .. } => {
                projection.add_usage_at(price, usage);
                folded += 1;
            }
            _ => {}
        }
    }
    folded
}

/// The `(provider, model)` the session was created with, if the snapshot names one.
fn route_of(snapshot: &serde_json::Value) -> Option<(String, String)> {
    ModelRoute::from_value(snapshot).map(|route| (route.provider, route.model))
}

/// The `config_snapshot` of the session's `SessionCreated` event (kept raw under
/// [`EventData::Other`]).
fn session_snapshot(session: &Session) -> serde_json::Value {
    for event in session.events() {
        if let EventData::Other(data) = &event.data
            && let Some(snapshot) = data.get("config_snapshot")
        {
            return snapshot.clone();
        }
    }
    serde_json::Value::Null
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::ModelUsage;

    /// The catalog prices from the research's §3.2 example (`opencode-go` /
    /// `deepseek-v4.1-flash`, micro-dollars per million tokens).
    fn deepseek() -> ModelPrice {
        ModelPrice {
            input: 150_000,          // $0.15 / Mtok
            output: 600_000,         // $0.60 / Mtok
            cache_read: Some(3_000), // $0.003 / Mtok
            cache_write: None,
        }
    }

    /// `claude-sonnet-4-5`: $3 in / $15 out / $0.30 cache read / $3.75 cache write.
    fn sonnet() -> ModelPrice {
        ModelPrice {
            input: 3_000_000,
            output: 15_000_000,
            cache_read: Some(300_000),
            cache_write: Some(3_750_000),
        }
    }

    /// The research's central number: a 200 000-token prompt with 190 000 of it cached. The
    /// cached 19/20 must be billed **once, at the cache rate** — billing `input` as reported
    /// would charge them again at full price ($0.03177 instead of $0.00327, 9.7x).
    #[test]
    fn an_openai_shaped_cache_hit_is_billed_once_at_the_cache_rate() {
        let usage = ModelUsage {
            input: Some(200_000),
            output: Some(2_000),
            cache_read: Some(190_000),
            input_includes_cache: Some(true),
            ..ModelUsage::default()
        };
        // (200000-190000) x $0.15 + 2000 x $0.60 + 190000 x $0.003, per Mtok.
        assert_eq!(cost_micros(deepseek(), &usage), Some(3_270));
    }

    /// The same shape on the Anthropic wire: `input` is already the uncached part, so it is
    /// billed as it stands and the cache fields are billed beside it. Both shapes land on the
    /// same fee for the same real request, which is the point of the flag.
    #[test]
    fn an_anthropic_shaped_request_with_the_same_cache_hit_costs_the_same() {
        let usage = ModelUsage {
            input: Some(10_000),
            output: Some(2_000),
            cache_read: Some(190_000),
            input_includes_cache: Some(false),
            ..ModelUsage::default()
        };
        // 10000 x $3 + 2000 x $15 + 190000 x $0.30, per Mtok = $0.117.
        assert_eq!(cost_micros(sonnet(), &usage), Some(117_000));
    }

    /// A cache **write** is billed at its own (higher) rate, and it is inside `input` for the
    /// OpenAI shape, so it must be subtracted there too.
    #[test]
    fn a_cache_write_is_billed_at_its_own_rate() {
        let usage = ModelUsage {
            input: Some(11_000),
            output: Some(2_000),
            cache_read: Some(1_000),
            cache_write: Some(10_000),
            input_includes_cache: Some(true),
            ..ModelUsage::default()
        };
        // uncached 0 x $3 + 2000 x $15 + 1000 x $0.30 + 10000 x $3.75 = $0.0678.
        assert_eq!(cost_micros(sonnet(), &usage), Some(67_800));
    }

    /// F7 (ADR 0027: never `$0.00`): an all-zero usage report, or no usage at all,
    /// has nothing to bill and hides the item.
    #[test]
    fn an_all_zero_report_has_nothing_to_bill() {
        let zeros = ModelUsage {
            input: Some(0),
            output: Some(0),
            cache_read: Some(0),
            cache_write: Some(0),
            input_includes_cache: Some(true),
            ..ModelUsage::default()
        };
        assert_eq!(cost_micros(deepseek(), &zeros), None);
        assert_eq!(cost_micros(sonnet(), &zeros), None);
        // No reported counters at all is the same case.
        assert_eq!(cost_micros(deepseek(), &ModelUsage::default()), None);
        // A zero cache reading is not a billed counter either; the input alone still is.
        let input_only = ModelUsage {
            input: Some(1_000_000),
            cache_read: Some(0),
            ..ModelUsage::default()
        };
        assert_eq!(cost_micros(sonnet(), &input_only), Some(3_000_000));
    }

    /// A counter the catalog has no rate for cannot be priced. If it is the only counter the
    /// call reported, the fee is unknown and the item hides — we do not bill the other
    /// counters and quietly call the total complete.
    #[test]
    fn a_counter_with_no_rate_makes_the_fee_unknown() {
        let cache_only = ModelUsage {
            cache_read: Some(190_000),
            input_includes_cache: Some(false),
            ..ModelUsage::default()
        };
        let no_cache_price = ModelPrice {
            input: 150_000,
            output: 600_000,
            cache_read: None,
            cache_write: None,
        };
        assert_eq!(cost_micros(no_cache_price, &cache_only), None);
    }

    /// The running total: `None` until a call has something to bill, and each call is priced
    /// at the price it was passed, so a `/model` switch mid-session does not re-price history.
    #[test]
    fn the_running_total_prices_each_call_at_its_own_price() {
        let cheap = ModelUsage {
            input: Some(1_000_000),
            input_includes_cache: Some(false),
            ..ModelUsage::default()
        };
        let expensive = cheap.clone();

        // Nothing billed yet: no fee, not zero.
        assert_eq!(add_cost(None, Some(sonnet()), &ModelUsage::default()), None);
        // No price at all: the call contributes nothing.
        assert_eq!(add_cost(None, None, &cheap), None);

        let mut total = add_cost(None, Some(deepseek()), &cheap);
        assert_eq!(total, Some(150_000), "$0.15");
        // The switch: the second call is priced with the new model, the first keeps its own.
        total = add_cost(total, Some(sonnet()), &expensive);
        assert_eq!(total, Some(3_150_000), "$0.15 + $3.00");
        // A later unpriced call does not erase what was already counted.
        total = add_cost(total, None, &cheap);
        assert_eq!(total, Some(3_150_000));
        // Saturating: an absurd price cannot wrap the total.
        let absurd = ModelPrice {
            input: u64::MAX,
            output: u64::MAX,
            cache_read: None,
            cache_write: None,
        };
        assert_eq!(
            add_cost(Some(u64::MAX), Some(absurd), &cheap),
            Some(u64::MAX)
        );
    }
}
