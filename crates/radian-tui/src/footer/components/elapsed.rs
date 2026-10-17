//! The `elapsed` component (ADR 0027; the clock is injected, revision 1).

use crate::app::App;
use crate::footer::Item;
use crate::theme::Role;

use super::{Component, ItemData};

pub(crate) struct Elapsed;

impl Component for Elapsed {
    fn id(&self) -> &str {
        "elapsed"
    }

    fn placeholders(&self) -> &'static [&'static str] {
        &["elapsed"]
    }

    fn glyphs(&self) -> [&'static str; 4] {
        // The unicode icon was dropped (ADR 0027: no covered glyph).
        ["", "\u{f017} ", "t ", "⏱ "]
    }

    fn priority(&self) -> i64 {
        35
    }

    fn role(&self, _app: &App) -> Option<Role> {
        Some(Role::Truncation)
    }

    fn data(&self, app: &App, _item: &Item) -> Option<ItemData> {
        let elapsed = crate::timing::format_duration(app.session.timer.elapsed(app.now)?);
        Some(ItemData::new("{elapsed}").value("elapsed", elapsed))
    }
}
