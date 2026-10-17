//! The `scroll` component (ADR 0027).

use crate::app::App;
use crate::footer::Item;

use super::{Component, ItemData};

pub(crate) struct Scroll;

impl Component for Scroll {
    fn id(&self) -> &str {
        "scroll"
    }

    fn placeholders(&self) -> &'static [&'static str] {
        &["position", "mode"]
    }

    fn glyphs(&self) -> [&'static str; 4] {
        ["", "", "", ""]
    }

    fn priority(&self) -> i64 {
        20
    }

    fn data(&self, app: &App, _item: &Item) -> Option<ItemData> {
        let position = if app.session.max_scroll == 0 {
            "all".to_string()
        } else {
            format!("{}/{}", app.session.scroll, app.session.max_scroll)
        };
        Some(
            ItemData::new("{position} {mode}")
                .value("position", position)
                .value(
                    "mode",
                    if app.session.follow {
                        "follow"
                    } else {
                        "paused"
                    }
                    .to_string(),
                ),
        )
    }
}
