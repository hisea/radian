//! ADR 0027: the TUI times a generation locally for the `speed` and `elapsed` footer
//! items, and formats the numbers those items show.
//!
//! **No wall clock here.** Every entry point takes the current `Instant` as an argument, so a
//! test injects time and a redraw never reads the clock: the loop sets [`crate::app::App::now`]
//! once per iteration. The token count is the model's own reported `usage.output` (never a
//! fabricated estimate from streamed characters); the duration is measured between the request's
//! start and its response, both of which the loop stamps.

use std::time::{Duration, Instant};

/// The live turn timer behind `elapsed` and `speed`.
#[derive(Debug, Default, Clone)]
pub struct TurnTimer {
    /// The running generation's start, or `None` when no generation is in flight.
    started: Option<Instant>,
    /// The last finished generation's output tokens, from the model's usage report.
    last_output: Option<u64>,
    /// The last finished generation's measured duration (for the idle `elapsed` reading).
    last_elapsed: Option<Duration>,
}

impl TurnTimer {
    /// The model request started at `now`.
    pub fn request_started(&mut self, now: Instant) {
        self.started = Some(now);
    }

    /// The model responded at `now`; `output` is its reported output tokens (or `None` when it
    /// reported no usage). Returns the measured duration, or `None` for a response with no
    /// matching start (a replay, or a response that arrived without the start event).
    pub fn response_finished(&mut self, now: Instant, output: Option<u64>) -> Option<Duration> {
        let started = self.started.take()?;
        let elapsed = now.saturating_duration_since(started);
        self.last_elapsed = Some(elapsed);
        if output.is_some() {
            // A response that reported no usage keeps the previous count: never invent one.
            self.last_output = output;
        }
        Some(elapsed)
    }

    /// Whether a generation is in flight (the `elapsed` item ticks).
    pub fn running(&self) -> bool {
        self.started.is_some()
    }

    /// The elapsed time to show at `now`: the running generation's live value, else the last
    /// finished generation's duration. `None` before any generation (a resume).
    pub fn elapsed(&self, now: Instant) -> Option<Duration> {
        self.started
            .map(|started| now.saturating_duration_since(started))
            .or(self.last_elapsed)
    }

    /// The tokens-per-second to show at `now`: the **last finished** generation's reported
    /// output tokens over its measured duration. `None` when there is nothing honest to show
    /// (no reported tokens, or no measured time) — so a zero-output generation hides the item
    /// rather than dividing by zero or showing `0.0 tok/s`.
    pub fn speed(&self, _now: Instant) -> Option<f64> {
        let output = self.last_output?;
        let elapsed = self.last_elapsed?;
        let seconds = elapsed.as_secs_f64();
        (output > 0 && seconds > 0.0).then(|| output as f64 / seconds)
    }
}

/// One generation's cost in micro-dollars, from the session's token totals and the model's
/// catalog price (per million tokens). `None` when the price is unknown or no token count was
/// reported: the item hides rather than showing `$0.00` for a model with no price.
pub fn cost_micros(
    price: Option<radian_host::ModelPrice>,
    input: Option<u64>,
    output: Option<u64>,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
) -> Option<u64> {
    let price = price?;
    let mut total: u64 = 0;
    let mut priced = false;
    let mut add = |tokens: Option<u64>, rate: Option<u64>| {
        if let (Some(tokens), Some(rate)) = (tokens, rate) {
            total = total.saturating_add(tokens.saturating_mul(rate) / 1_000_000);
            priced = true;
        }
    };
    add(input, Some(price.input));
    add(output, Some(price.output));
    add(cache_read, price.cache_read);
    add(cache_write, price.cache_write);
    priced.then_some(total)
}

/// `16.2s`, `1m02s` past a minute, `1h02m` past an hour.
pub fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs_f64();
    if duration.as_secs() < 60 {
        format!("{seconds:.1}s")
    } else if duration.as_secs() < 3600 {
        format!(
            "{}m{:02}s",
            duration.as_secs() / 60,
            duration.as_secs() % 60
        )
    } else {
        format!(
            "{}h{:02}m",
            duration.as_secs() / 3600,
            (duration.as_secs() % 3600) / 60
        )
    }
}

/// `42.1 tok/s`.
pub fn format_speed(tokens_per_second: f64) -> String {
    format!("{tokens_per_second:.1} tok/s")
}

/// A token count: `760`, `19.0K`, `1.2M`. Kept short so the numbers row fits.
pub fn format_tokens(tokens: u64) -> String {
    if tokens < 1_000 {
        tokens.to_string()
    } else if tokens < 1_000_000 {
        compact(tokens, 1_000, "K")
    } else {
        compact(tokens, 1_000_000, "M")
    }
}

/// `n / unit` with one decimal, dropping a trailing `.0` (`19000` → `19K`).
fn compact(tokens: u64, unit: u64, suffix: &str) -> String {
    let value = tokens as f64 / unit as f64;
    let text = format!("{value:.1}");
    let text = text.strip_suffix(".0").unwrap_or(&text);
    format!("{text}{suffix}")
}

/// `$0.10`, `$1.23`. Sub-cent values keep more precision (`$0.0030`) rather than rounding to
/// `$0.00`, which would read as "free".
pub fn format_cost(micros: u64) -> String {
    if micros > 0 && micros < 10_000 {
        format!("${:.4}", micros as f64 / 1_000_000.0)
    } else {
        format!("${:.2}", micros as f64 / 1_000_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, millis: u64) -> Instant {
        base + Duration::from_millis(millis)
    }

    /// The elapsed value is live while a generation runs, and the last one when idle; the
    /// speed is the last finished generation's tokens over its measured time.
    #[test]
    fn the_timer_tracks_a_live_elapsed_and_a_finished_speed() {
        let base = Instant::now();
        let mut timer = TurnTimer::default();
        assert_eq!(timer.elapsed(base), None, "nothing before a generation");
        assert_eq!(timer.speed(base), None);

        timer.request_started(base);
        assert!(timer.running());
        assert_eq!(
            timer.elapsed(at(base, 500)),
            Some(Duration::from_millis(500))
        );
        assert_eq!(timer.speed(at(base, 500)), None, "no count yet");

        // The model reports 100 output tokens after 2 s: 50 tok/s.
        let elapsed = timer.response_finished(at(base, 2_000), Some(100));
        assert_eq!(elapsed, Some(Duration::from_secs(2)));
        assert!(!timer.running());
        assert_eq!(timer.elapsed(at(base, 9_000)), Some(Duration::from_secs(2)));
        assert_eq!(timer.speed(at(base, 9_000)), Some(50.0));
    }

    /// A response with no reported usage keeps the previous token count, and a zero-output
    /// generation hides the speed instead of dividing by zero (ADR 0027).
    #[test]
    fn a_zero_output_generation_hides_the_speed() {
        let base = Instant::now();
        let mut timer = TurnTimer::default();
        timer.request_started(base);
        timer.response_finished(at(base, 1_000), Some(0));
        assert_eq!(timer.speed(at(base, 2_000)), None);
        // No time measured (a zero-length generation) is the other divide-by-zero guard.
        timer.request_started(base);
        timer.response_finished(base, Some(10));
        assert_eq!(timer.speed(base), None);
    }

    /// The cost is computed from the catalog price (per million tokens) and the reported
    /// totals; an unknown price or no token count hides it.
    #[test]
    fn the_cost_uses_the_catalog_price() {
        let price = radian_host::ModelPrice {
            input: 3_000_000,   // $3 / Mtok
            output: 15_000_000, // $15 / Mtok
            cache_read: Some(300_000),
            cache_write: None,
        };
        // 1M input ($3) + 100K output ($1.5) + 500K cache read ($0.15) = $4.65.
        assert_eq!(
            cost_micros(
                Some(price),
                Some(1_000_000),
                Some(100_000),
                Some(500_000),
                None
            ),
            Some(4_650_000)
        );
        // No price at all: no cost, and never `$0.00`.
        assert_eq!(cost_micros(None, Some(10), Some(2), None, None), None);
        // A price but no token count: same.
        assert_eq!(cost_micros(Some(price), None, None, None, None), None);
    }

    /// The three number formats.
    #[test]
    fn the_numbers_format_short() {
        assert_eq!(format_duration(Duration::from_millis(16_200)), "16.2s");
        assert_eq!(format_duration(Duration::from_secs(62)), "1m02s");
        assert_eq!(format_duration(Duration::from_secs(3_720)), "1h02m");
        assert_eq!(format_speed(42.14), "42.1 tok/s");
        assert_eq!(format_tokens(760), "760");
        assert_eq!(format_tokens(19_000), "19K");
        assert_eq!(format_tokens(1_200_000), "1.2M");
        assert_eq!(format_cost(100_000), "$0.10");
        assert_eq!(format_cost(3_000), "$0.0030");
    }
}
