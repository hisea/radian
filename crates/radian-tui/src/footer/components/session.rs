//! The `session` component (ADR 0027).

use crate::app::App;
use crate::footer::Item;

use super::{Component, ItemData};

pub(crate) struct Session;

impl Component for Session {
    fn id(&self) -> &str {
        "session"
    }

    fn placeholders(&self) -> &'static [&'static str] {
        &["id"]
    }

    fn glyphs(&self) -> [&'static str; 4] {
        ["# ", "", "# ", "# "]
    }

    fn priority(&self) -> i64 {
        65
    }

    fn data(&self, app: &App, _item: &Item) -> Option<ItemData> {
        Some(ItemData::new("{id}").value("id", app.session.session_id.clone()))
    }
}
