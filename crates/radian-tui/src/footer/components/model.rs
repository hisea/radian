//! The `model` component (ADR 0027; the display name is ADR 0027 C2).

use crate::app::App;
use crate::footer::Item;
use crate::theme::Role;

use super::{Component, ItemData};

pub(crate) struct Model;

impl Component for Model {
    fn id(&self) -> &str {
        "model"
    }

    fn placeholders(&self) -> &'static [&'static str] {
        &["display_name", "id", "provider"]
    }

    fn glyphs(&self) -> [&'static str; 4] {
        ["", "", "", ""]
    }

    fn role(&self, _app: &App) -> Option<Role> {
        Some(Role::Model)
    }

    fn data(&self, app: &App, _item: &Item) -> Option<ItemData> {
        Some(
            ItemData::new("{display_name}")
                .value(
                    "display_name",
                    app.session
                        .model_display
                        .clone()
                        .unwrap_or_else(|| app.session.model_name.clone()),
                )
                .value("id", app.session.model_name.clone())
                .value("provider", app.session.provider.clone().unwrap_or_default()),
        )
    }
}
