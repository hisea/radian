//! The `host` component (ADR 0027; the hostname is startup fact).

use crate::app::App;
use crate::footer::Item;
use crate::theme::Role;

use super::{Component, ItemData};

pub(crate) struct Host;

impl Component for Host {
    fn id(&self) -> &str {
        "host"
    }

    fn placeholders(&self) -> &'static [&'static str] {
        &["host"]
    }

    fn glyphs(&self) -> [&'static str; 4] {
        // ADR 0027: `@`, the nerd `f109`, the word `host`; `⌂` is not in all six
        // reference fonts.
        ["@ ", "\u{f109} ", "host ", ""]
    }

    fn priority(&self) -> i64 {
        // ADR 0027: the pre-refactor `item_priority` table's value.
        78
    }

    fn role(&self, _app: &App) -> Option<Role> {
        Some(Role::NoticeBody)
    }

    fn data(&self, app: &App, _item: &Item) -> Option<ItemData> {
        Some(ItemData::new("{host}").value("host", app.hostname.clone()?))
    }
}
