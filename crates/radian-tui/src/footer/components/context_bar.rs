//! The `context_bar` component (ADR 0027; width/fill/empty are C2).
//!
//! Slice ③ merges this id into `context`; for slice ① it is its own component, so the old id
//! and its parameters keep working byte-for-byte.

use crate::app::App;
use crate::footer::{Item, auto_marker, context_fraction};
use crate::theme::Role;

use super::{Component, ItemData};

pub(crate) struct ContextBar;

/// The context bar: `width` cells, `fill` for the used part and `empty` for the rest.
fn render_bar(item: &Item, fraction: f64) -> String {
    let param = |key: &str| {
        item.params
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    };
    let width: usize = param("width")
        .and_then(|value| value.parse().ok())
        .unwrap_or(10);
    let fill = param("fill").unwrap_or("━");
    let empty = param("empty").unwrap_or("─");
    let used = ((fraction.clamp(0.0, 1.0)) * width as f64).round() as usize;
    let used = used.min(width);
    format!("{}{}", fill.repeat(used), empty.repeat(width - used))
}

impl Component for ContextBar {
    fn id(&self) -> &str {
        "context_bar"
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

    fn data(&self, app: &App, item: &Item) -> Option<ItemData> {
        let session = app.session.projection.read();
        let budget = session.context_window();
        let report = session.budget();
        let used = report.map(|report| report.projected_tokens);
        let fraction = context_fraction(&session).unwrap_or(0.0);
        let percent = format!("{:.1}", fraction * 100.0);
        Some(
            ItemData::new("{bar} {percent}%")
                .value("bar", render_bar(item, fraction))
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
