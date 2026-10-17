//! The `status` component (ADR 0027; C2/C3). It lists every `status.*`
//! contribution, filtered by its `keys = ["…"]` parameter when it has one.

use radian_host::SegmentRole;

use crate::app::App;
use crate::footer::{Item, contrib_role, role_style};
use crate::theme::Role;

use super::{Component, ItemData};

pub(crate) struct Status;

impl Component for Status {
    fn id(&self) -> &str {
        "status"
    }

    fn placeholders(&self) -> &'static [&'static str] {
        &["status"]
    }

    fn glyphs(&self) -> [&'static str; 4] {
        ["", "", "", ""]
    }

    fn priority(&self) -> i64 {
        45
    }

    fn data(&self, app: &App, item: &Item) -> Option<ItemData> {
        let keys: Option<Vec<&str>> = item
            .params
            .iter()
            .find(|(name, _)| name == "keys")
            .map(|(_, value)| value.split(',').map(str::trim).collect());
        let mut parts = Vec::new();
        let mut stale = false;
        for contribution in app.contributions.snapshot() {
            let Some(key) = contribution.item.strip_prefix("status.") else {
                continue;
            };
            if keys.as_ref().is_some_and(|keys| !keys.contains(&key)) {
                continue;
            }
            let text: String = contribution
                .segments
                .iter()
                .map(|segment| segment.text.as_str())
                .collect();
            if !text.is_empty() {
                parts.push(text);
            }
            stale |= contribution.stale;
        }
        if parts.is_empty() {
            return None;
        }
        let role = if stale {
            Role::Truncation
        } else {
            contrib_role(SegmentRole::Normal)
        };
        Some(
            ItemData::new("{status}")
                .value("status", parts.join(" "))
                .fixed_style(role_style(app, role)),
        )
    }
}
