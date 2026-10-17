//! The `pending` component (ADR 0027).

use crate::app::App;
use crate::footer::Item;
use crate::theme::Role;

use super::{Component, ItemData};

pub(crate) struct Pending;

impl Component for Pending {
    fn id(&self) -> &str {
        "pending"
    }

    fn placeholders(&self) -> &'static [&'static str] {
        &["count"]
    }

    fn glyphs(&self) -> [&'static str; 4] {
        ["", "", "", ""]
    }

    fn priority(&self) -> i64 {
        70
    }

    fn role(&self, _app: &App) -> Option<Role> {
        Some(Role::TurnRunning)
    }

    fn data(&self, app: &App, _item: &Item) -> Option<ItemData> {
        (app.pending > 0).then(|| {
            ItemData::new("{count} pending")
                .value("count", app.pending.to_string())
                .numeric(Some(app.pending as u64))
        })
    }
}
