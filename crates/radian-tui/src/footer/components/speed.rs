//! The `speed` component (ADR 0027; the clock is injected).

use crate::app::App;
use crate::footer::Item;
use crate::theme::Role;

use super::{Component, ItemData};

pub(crate) struct Speed;

impl Component for Speed {
    fn id(&self) -> &str {
        "speed"
    }

    fn placeholders(&self) -> &'static [&'static str] {
        &["speed"]
    }

    fn glyphs(&self) -> [&'static str; 4] {
        ["\u{bb} ", "\u{f0e7} ", "spd ", "🚀 "]
    }

    fn priority(&self) -> i64 {
        55
    }

    fn role(&self, _app: &App) -> Option<Role> {
        Some(Role::Usage)
    }

    fn data(&self, app: &App, _item: &Item) -> Option<ItemData> {
        let speed = crate::timing::format_speed(app.session.timer.speed(app.now)?);
        Some(ItemData::new("{speed}").value("speed", speed))
    }
}
