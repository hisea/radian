//! The `mcp` component (ADR 0027).

use crate::app::App;
use crate::footer::Item;
use crate::theme::Role;

use super::{Component, ItemData};

pub(crate) struct Mcp;

impl Component for Mcp {
    fn id(&self) -> &str {
        "mcp"
    }

    fn placeholders(&self) -> &'static [&'static str] {
        &["mcp"]
    }

    fn glyphs(&self) -> [&'static str; 4] {
        ["", "", "", ""]
    }

    fn priority(&self) -> i64 {
        55
    }

    fn role(&self, _app: &App) -> Option<Role> {
        Some(Role::Mcp)
    }

    fn data(&self, app: &App, _item: &Item) -> Option<ItemData> {
        let session = app.session.projection.read();
        Some(ItemData::new("{mcp}").value(
            "mcp",
            crate::app::mcp_text(session.mcp_configured(), session.mcp_open()),
        ))
    }
}
