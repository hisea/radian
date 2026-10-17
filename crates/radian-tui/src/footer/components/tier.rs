//! The `tier` component (ADR 0027).

use crate::app::App;
use crate::footer::Item;
use crate::theme::Role;

use super::{Component, ItemData};

pub(crate) struct Tier;

impl Component for Tier {
    fn id(&self) -> &str {
        "tier"
    }

    fn placeholders(&self) -> &'static [&'static str] {
        &["tier"]
    }

    fn glyphs(&self) -> [&'static str; 4] {
        ["", "", "", ""]
    }

    fn priority(&self) -> i64 {
        75
    }

    fn role(&self, _app: &App) -> Option<Role> {
        Some(Role::Tier)
    }

    fn data(&self, app: &App, _item: &Item) -> Option<ItemData> {
        let session = app.session.projection.read();
        let tier = session.tier()?;
        Some(ItemData::new("{tier}").value("tier", tier.to_string()))
    }
}
