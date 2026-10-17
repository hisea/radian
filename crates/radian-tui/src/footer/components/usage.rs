//! The `usage` component (ADR 0027): the current provider's usage, as
//! one line.
//!
//! It is a **provider-related** component: its data is the `usage` item's contribution — the
//! [`UsageSnapshot`] the fetch a provider's descriptor declares wrote — and it knows no
//! provider by name. It draws only while the current route's provider declares a usage source
//! *and* the stored snapshot is that provider's own, so `/model`-ing to a provider without one
//! hides the row even while the store still holds an older value.

use radian_host::UsageSnapshot;

use crate::app::App;
use crate::footer::{Item, role_style};
use crate::theme::Role;

use super::{Component, ItemData};

pub(crate) struct Usage;

impl Component for Usage {
    fn id(&self) -> &str {
        "usage"
    }

    fn placeholders(&self) -> &'static [&'static str] {
        // The line is the whole value, like a `command` item's: there is nothing to validate.
        &["value"]
    }

    fn glyphs(&self) -> [&'static str; 4] {
        ["", "", "", ""]
    }

    fn data(&self, app: &App, _item: &Item) -> Option<ItemData> {
        let provider = app.session.provider.as_deref()?;
        // ADR 0027: a descriptor with no `usage` source draws nothing.
        radian_host::usage_endpoint(provider)?;
        let contribution = app.contributions.get(radian_host::USAGE_ITEM)?;
        let snapshot = contribution.usage.as_ref()?;
        if snapshot.provider != provider {
            return None;
        }
        // A stale snapshot is the last good value, drawn dim (ADR 0027 C3) — the same style the
        // pre-refactor store fallback gave the row.
        let role = if contribution.stale {
            Role::Truncation
        } else {
            Role::Text
        };
        Some(
            ItemData::new("{value}")
                .value("value", line(snapshot))
                .fixed_style(role_style(app, role)),
        )
    }
}

/// The snapshot as the footer's one line: every window's **remaining** share, then every
/// window's reset countdown, then the provider's trailing word. Byte-identical to the line the
/// two sources built themselves before the move (ADR 0027 slice ②'s acceptance),
/// which is why the separator here is the sources' own ` · ` and not the footer's.
fn line(snapshot: &UsageSnapshot) -> String {
    let mut shares = Vec::new();
    let mut resets = Vec::new();
    for window in &snapshot.windows {
        if let Some(remaining) = &window.remaining {
            shares.push(format!("{}: {}", window.label, remaining.text()));
        }
        if let Some(reset) = &window.resets_at {
            resets.push(format!("{} ↺{reset}", window.label));
        }
    }
    let mut out = format!("{} (remaining): ", snapshot.title);
    out.push_str(&shares.join(" · "));
    if !resets.is_empty() {
        out.push_str(" · ");
        out.push_str(&resets.join(" · "));
    }
    if let Some(plan) = &snapshot.plan {
        out.push_str(&format!(" · {plan}"));
    }
    out
}
