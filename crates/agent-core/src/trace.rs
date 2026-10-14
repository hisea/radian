//! A phase trace for startup measurements, off unless `RADIAN_TRACE_PHASES` is set.
//!
//! The env var is read once (`OnceLock`), so every call site is a single branch when the trace
//! is off. The TUI hosts, the session reader and the runtime driver all use this one helper
//! instead of keeping their own copy of the flag and the clock.

use std::sync::OnceLock;
use std::time::Instant;

static ENABLED: OnceLock<bool> = OnceLock::new();
static START: OnceLock<Instant> = OnceLock::new();

/// Whether the trace is on (`RADIAN_TRACE_PHASES` set). Reads the environment once.
pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| std::env::var_os("RADIAN_TRACE_PHASES").is_some())
}

/// Starts the phase clock. The first call wins, so a process can call it from more than one
/// entry point. No-op when the trace is off.
pub fn start() {
    if enabled() {
        let _ = START.set(Instant::now());
    }
}

/// Prints `[trace] <label> <ms>` measured from [`start`] (or from the first `mark` when
/// `start` never ran). No-op when the trace is off.
pub fn mark(label: &str) {
    if !enabled() {
        return;
    }
    let start = *START.get_or_init(Instant::now);
    eprintln!("[trace] {label} {} ms", start.elapsed().as_millis());
}

/// Prints `[trace] <label> <ms>` for a step measured against a local `Instant` (a nested
/// measurement that does not want the global clock). No-op when the trace is off.
pub fn took(label: &str, from: Instant) {
    if enabled() {
        eprintln!("[trace] {label} {} ms", from.elapsed().as_millis());
    }
}
