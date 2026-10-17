//! The `context` component (ADR 0027; the placeholders are C2/revision 1).
//!
//! Slice ③ merges `context_bar` into this component; for slice ① the two stay separate so the
//! render output is byte-for-byte unchanged (`context`'s `{bar}` is empty, as before).

use crate::app::App;
use crate::footer::{Item, auto_marker, context_fraction};
use crate::theme::Role;

use super::{Component, ItemData};

pub(crate) struct Context;

impl Component for Context {
    fn id(&self) -> &str {
        "context"
    }

    fn placeholders(&self) -> &'static [&'static str] {
        &["bar", "percent", "used", "budget", "auto"]
    }

    fn glyphs(&self) -> [&'static str; 4] {
        ["", "\u{f035b} ", "[", ""]
    }

    fn priority(&self) -> i64 {
        85
    }

    fn role(&self, _app: &App) -> Option<Role> {
        Some(Role::Budget)
    }

    fn thresholds(&self) -> Vec<(i64, crate::footer::ItemStyle)> {
        vec![
            (70, crate::footer::ItemStyle::Role(Role::BudgetOver)),
            (90, crate::footer::ItemStyle::Role(Role::ToolResultError)),
        ]
    }

    fn data(&self, app: &App, _item: &Item) -> Option<ItemData> {
        let session = app.session.projection.read();
        let budget = session.context_window();
        let report = session.budget();
        let used = report.map(|report| report.projected_tokens);
        let fraction = context_fraction(&session).unwrap_or(0.0);
        // One decimal (ADR 0027): `41.3%`.
        let percent = format!("{:.1}", fraction * 100.0);
        Some(
            ItemData::new("{used}/{budget}")
                .value("bar", String::new())
                .value("auto", auto_marker(app))
                .value("percent", percent)
                .value(
                    "used",
                    used.map_or_else(|| "-".to_string(), crate::timing::format_tokens),
                )
                .value(
                    "budget",
                    budget.map_or_else(|| "-".to_string(), crate::timing::format_tokens),
                )
                .numeric(report.map(|_| (fraction * 100.0).round() as u64)),
        )
    }
}
