//! The `tasks` component (ADR 0027; ADR 0030 O10).

use crate::app::App;
use crate::footer::Item;
use crate::theme::Role;

use super::{Component, ItemData};

pub(crate) struct Tasks;

impl Component for Tasks {
    fn id(&self) -> &str {
        "tasks"
    }

    fn placeholders(&self) -> &'static [&'static str] {
        &["running", "total"]
    }

    fn glyphs(&self) -> [&'static str; 4] {
        ["", "", "", ""]
    }

    fn priority(&self) -> i64 {
        72
    }

    fn role(&self, _app: &App) -> Option<Role> {
        Some(Role::TurnRunning)
    }

    fn data(&self, app: &App, _item: &Item) -> Option<ItemData> {
        // Absent with no tasks, so it never reads `0/0`.
        app.session.task_counts().map(|(running, total)| {
            ItemData::new("tasks {running}/{total}")
                .value("running", running.to_string())
                .value("total", total.to_string())
                .numeric(Some(total as u64))
        })
    }
}
