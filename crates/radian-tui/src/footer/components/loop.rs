//! The `loop` component: the `/loop` continuation's state (backlog
//! `tui-loop-continuation.md`). `loop 3/20` while iterating, `loop every 5m · 3/20` with an
//! `--every` wait between turns, `loop · next in 3m` while the wait runs, `loop on` while
//! armed (no prompt yet), `loop paused` while paused. Empty — so its row takes no space —
//! while the loop is off.

use crate::app::App;
use crate::footer::Item;
use crate::theme::Role;

use super::{Component, ItemData};

pub(crate) struct Loop;

impl Component for Loop {
    fn id(&self) -> &str {
        "loop"
    }

    fn placeholders(&self) -> &'static [&'static str] {
        &["count", "every", "next"]
    }

    fn glyphs(&self) -> [&'static str; 4] {
        // No icon anywhere: the state is the whole text, like `queued`'s line.
        ["", "", "", ""]
    }

    fn priority(&self) -> i64 {
        // A running loop is as worth keeping on a narrow terminal as a pending request.
        70
    }

    fn role(&self, _app: &App) -> Option<Role> {
        Some(Role::TurnRunning)
    }

    fn data(&self, app: &App, _item: &Item) -> Option<ItemData> {
        let state = &app.session.loop_state;
        if !state.active {
            return None;
        }
        if state.paused {
            return Some(ItemData::new("loop paused"));
        }
        if state.prompt.is_none() {
            return Some(ItemData::new("loop on"));
        }
        let count = format!("{}/{}", state.iterations, state.max_iterations);
        if let Some(due) = state.next_due {
            let remaining = due.saturating_duration_since(app.now);
            return Some(
                ItemData::new("loop · next in {next}")
                    .value(
                        "next",
                        crate::app::loop_mode::format_loop_duration(remaining),
                    )
                    .value("count", count),
            );
        }
        let mut data = ItemData::new("loop {count}").value("count", count);
        if let Some(every) = state.every {
            data = ItemData::new("loop every {every} · {count}")
                .value("every", crate::app::loop_mode::format_loop_duration(every))
                .value(
                    "count",
                    format!("{}/{}", state.iterations, state.max_iterations),
                );
        }
        Some(data)
    }
}
