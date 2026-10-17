//! The `brand` component (ADR 0027; the mark is ADR 0027/8).

use crate::app::App;
use crate::footer::{Glyphs, Item};
use crate::theme::Role;

use super::{Component, ItemData};

pub(crate) struct Brand;

impl Component for Brand {
    fn id(&self) -> &str {
        "brand"
    }

    fn placeholders(&self) -> &'static [&'static str] {
        &["brand"]
    }

    fn glyphs(&self) -> [&'static str; 4] {
        // The brand mark *is* the value, so it carries no separate icon.
        ["", "", "", ""]
    }

    fn role(&self, app: &App) -> Option<Role> {
        // ADR 0027/8: the brand is a bold accent mark. The theme's accent role is
        // `notice_label`, so the mark follows a theme switch; a configured `tui.brand` keeps
        // this style in every glyph set.
        Some(
            if app.footer_glyphs() == Glyphs::Unicode || app.brand.is_some() {
                Role::NoticeLabel
            } else {
                Role::Text
            },
        )
    }

    fn data(&self, app: &App, _item: &Item) -> Option<ItemData> {
        // ADR 0027/8: `tui.brand` when set, else the brand mark per glyph set.
        Some(ItemData::new("{brand}").value("brand", app.brand_mark()))
    }
}
