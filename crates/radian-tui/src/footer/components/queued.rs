//! The `queued` component: the `next_turn` inputs this session sent that the runtime has
//! not admitted yet (`/queue`, and an idle Enter while its turn has not started).
//!
//! The data is the enqueue/admission pair `apply_runtime_event` keeps on the session (one
//! entry per `InboxEnqueued` without a matching `InputAdmitted`, by message id); empty
//! when nothing waits, so its row takes no space.

use crate::app::App;
use crate::footer::Item;
use crate::theme::Role;

use super::{Component, ItemData};

pub(crate) struct Queued;

impl Component for Queued {
    fn id(&self) -> &str {
        "queued"
    }

    fn placeholders(&self) -> &'static [&'static str] {
        &["count"]
    }

    fn glyphs(&self) -> [&'static str; 4] {
        // No icon anywhere: the count is the whole text, like `pending`'s line.
        ["", "", "", ""]
    }

    fn priority(&self) -> i64 {
        // A waiting followup is as worth keeping on a narrow terminal as a pending request.
        70
    }

    fn role(&self, _app: &App) -> Option<Role> {
        Some(Role::TurnRunning)
    }

    fn data(&self, app: &App, _item: &Item) -> Option<ItemData> {
        let count = app.session.queued.len();
        (count > 0).then(|| {
            ItemData::new("{count} queued")
                .value("count", count.to_string())
                .numeric(Some(count as u64))
        })
    }
}
