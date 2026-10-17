//! The `jobs` component (ADR 0027's 2026-10-17 note, slice ③ item 5): this
//! session's running background jobs and the oldest one's age (for example `2 jobs · 18m`).
//!
//! The data is the start/settle pair the [`JobNotifier`](radian_host::JobNotifier) forwards on
//! the live runtime-event sink, kept on the session as `(id, started)`; empty when none runs,
//! so its row takes no space. The age is the loop's clock minus the start the TUI saw, and it
//! ticks with the existing footer refresh (a contributor tick redraws) — no new timer.

use std::time::Duration;

use crate::app::App;
use crate::footer::Item;

use super::{Component, ItemData};

pub(crate) struct Jobs;

impl Component for Jobs {
    fn id(&self) -> &str {
        "jobs"
    }

    fn placeholders(&self) -> &'static [&'static str] {
        &["count", "age"]
    }

    fn glyphs(&self) -> [&'static str; 4] {
        // No icon anywhere: the count and the age are the whole text, like `usage`'s line.
        ["", "", "", ""]
    }

    fn priority(&self) -> i64 {
        // A running job is as worth keeping on a narrow terminal as a pending request.
        70
    }

    fn data(&self, app: &App, _item: &Item) -> Option<ItemData> {
        let running = &app.session.running_jobs;
        if running.is_empty() {
            return None;
        }
        let oldest = running.iter().min_by_key(|job| job.started)?;
        let age = app.now.saturating_duration_since(oldest.started);
        let count = running.len();
        Some(
            ItemData::new("{count} · {age}")
                .value(
                    "count",
                    if count == 1 {
                        "1 job".to_owned()
                    } else {
                        format!("{count} jobs")
                    },
                )
                .value("age", format_age(age)),
        )
    }
}

/// A compact age: `5s` under a minute, `18m` under an hour, `2h14m` above.
fn format_age(age: Duration) -> String {
    let secs = age.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The age compacts to seconds, minutes and hours with no padding (ADR 0027's
    /// compact numbers: no label words, no zero units).
    #[test]
    fn the_age_is_compact() {
        assert_eq!(format_age(Duration::from_secs(5)), "5s");
        assert_eq!(format_age(Duration::from_secs(59)), "59s");
        assert_eq!(format_age(Duration::from_secs(60)), "1m");
        assert_eq!(format_age(Duration::from_secs(18 * 60 + 40)), "18m");
        assert_eq!(format_age(Duration::from_secs(2 * 3600 + 14 * 60)), "2h14m");
    }
}
