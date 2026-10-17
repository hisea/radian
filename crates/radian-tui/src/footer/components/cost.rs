//! The `cost` component (ADR 0027; the fee algorithm is revision 11/12).

use crate::app::App;
use crate::footer::{Item, ItemStyle};
use crate::theme::Role;

use super::{Component, ItemData};

pub(crate) struct Cost;

impl Component for Cost {
    fn id(&self) -> &str {
        "cost"
    }

    fn placeholders(&self) -> &'static [&'static str] {
        &["usd"]
    }

    fn glyphs(&self) -> [&'static str; 4] {
        ["", "", "", ""]
    }

    fn priority(&self) -> i64 {
        40
    }

    fn thresholds(&self) -> Vec<(i64, ItemStyle)> {
        vec![
            (70, ItemStyle::Role(Role::BudgetOver)),
            (90, ItemStyle::Role(Role::ToolResultError)),
        ]
    }

    fn data(&self, app: &App, _item: &Item) -> Option<ItemData> {
        let session = app.session.projection.read();
        let micros = session.cost_micros()?;
        // ADR 0027 and 3: a total with a missing rate is incomplete
        // (`≥`), and a total re-priced from old records is an estimate (`~`). The estimate wins
        // when both hold (`~≥` would be unreadable). A recorded, complete total has no marker.
        let marker = if session.cost_estimated() {
            "~"
        } else if session.cost_incomplete() {
            "≥"
        } else {
            ""
        };
        Some(
            ItemData::new("{usd}")
                .value(
                    "usd",
                    format!("{marker}{}", crate::timing::format_cost(micros)),
                )
                .numeric(Some(micros / 1_000_000)),
        )
    }
}
