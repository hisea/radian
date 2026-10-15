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

/// `12s`, `5m02s`, `1h02m`: the whole-second duration the `bar` input's status row shows
/// (ADR 0035 V3 writes it as `12s`). [`format_duration`] keeps a decimal for the footer's
/// `elapsed` item; a status line does not need sub-second precision.
pub fn format_seconds(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h{:02}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

/// `42.1 tok/s`.
pub fn format_speed(tokens_per_second: f64) -> String {
    format!("{tokens_per_second:.1} tok/s")
}

/// A token count: `760`, `19.0K`, `1.2M`. Kept short so the numbers row fits.
/// A token or window count, compact (ADR 0027): raw below 1000, `K` below a
/// million, `M` above; one decimal below ten of the unit, whole above it. `15_000_000` →
/// `15M`, `1_962_000_000` → `1962M`, `9_500` → `9.5K`, `999` → `999`.
pub fn format_tokens(tokens: u64) -> String {
    if tokens < 1_000 {
        return tokens.to_string();
    }
    let (unit, suffix) = if tokens < 1_000_000 {
        (1_000.0, "K")
    } else {
        (1_000_000.0, "M")
    };
    let value = tokens as f64 / unit;
    if value < 10.0 {
        format!("{value:.1}{suffix}")
    } else {
        format!("{}{suffix}", value.round() as u64)
    }
}

/// `$0.10`, `$1.23`. Sub-cent values keep more precision (`$0.0030`) rather than rounding to
/// `$0.00`, which would read as "free".
pub fn format_cost(micros: u64) -> String {
    // Three decimals (ADR 0027's sample `$14.510`), so a sub-cent cost is visible.
    format!("${:.3}", micros as f64 / 1_000_000.0)
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

    /// The three number formats.
    #[test]
    fn the_numbers_format_short() {
        assert_eq!(format_duration(Duration::from_millis(16_200)), "16.2s");
        assert_eq!(format_duration(Duration::from_secs(62)), "1m02s");
        assert_eq!(format_duration(Duration::from_secs(3_720)), "1h02m");
        // ADR 0035 V3's status-row clock is the whole-second form (`12s`, never `12.3s`).
        assert_eq!(format_seconds(Duration::from_millis(12_300)), "12s");
        assert_eq!(format_seconds(Duration::from_millis(900)), "0s");
        assert_eq!(format_seconds(Duration::from_secs(62)), "1m02s");
        assert_eq!(format_seconds(Duration::from_secs(3_720)), "1h02m");
        assert_eq!(format_speed(42.14), "42.1 tok/s");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_000), "1.0K");
        assert_eq!(format_tokens(9_500), "9.5K");
        assert_eq!(format_tokens(10_000), "10K");
        assert_eq!(format_tokens(15_000_000), "15M");
        assert_eq!(format_tokens(1_962_000_000), "1962M");
        assert_eq!(format_tokens(983_616), "984K"); // >=10 of the unit rounds (revision 3)
        assert_eq!(format_cost(100_000), "$0.100");
        assert_eq!(format_cost(14_510_000), "$14.510");
    }
}
