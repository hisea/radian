//! The `tokens` component (ADR 0027; `{cache_write}` is revision 4).

use crate::app::App;
use crate::footer::{Glyphs, Item};
use crate::theme::Role;

use super::{Component, ItemData};

pub(crate) struct Tokens;

impl Component for Tokens {
    fn id(&self) -> &str {
        "tokens"
    }

    fn placeholders(&self) -> &'static [&'static str] {
        &["in", "out", "cache", "cache_write", "in_arrow", "out_arrow"]
    }

    fn glyphs(&self) -> [&'static str; 4] {
        ["", "", "", ""]
    }

    fn role(&self, _app: &App) -> Option<Role> {
        Some(Role::Usage)
    }

    fn data(&self, app: &App, _item: &Item) -> Option<ItemData> {
        let session = app.session.projection.read();
        // The default format is a tier default (ADR 0027 item 2's unicode arrows,
        // or `^`/`v`). A preset or user-configured format that needs the arrows without the
        // default's `R` uses the `{in_arrow}`/`{out_arrow}` placeholders instead, so the
        // format stays one static string under every glyph set (the `full` preset does this).
        let (in_arrow, out_arrow) = match app.footer_glyphs() {
            Glyphs::Unicode | Glyphs::Emoji => ("↑", "↓"),
            Glyphs::Nerd | Glyphs::Ascii => ("^", "v"),
        };
        let default_format = match app.footer_glyphs() {
            Glyphs::Unicode | Glyphs::Emoji => "↑{in} ↓{out} R{cache}",
            Glyphs::Nerd | Glyphs::Ascii => "^{in} v{out} R{cache}",
        };
        Some(
            ItemData::new(default_format)
                .value("in_arrow", in_arrow)
                .value("out_arrow", out_arrow)
                .value(
                    "in",
                    session
                        .usage_input()
                        .map_or_else(|| "-".to_string(), crate::timing::format_tokens),
                )
                .value(
                    "out",
                    session
                        .usage_output()
                        .map_or_else(|| "-".to_string(), crate::timing::format_tokens),
                )
                // ADR 0027: the cache-read total. `-` when the provider never
                // reported one, never a fabricated 0.
                .value(
                    "cache",
                    session
                        .usage_cache_read()
                        .map_or_else(|| "-".to_string(), crate::timing::format_tokens),
                )
                // F8 (ADR 0027): the cache-**write** total. It is billed at its own
                // (often higher) rate, so a format may show it — but it is deliberately **not**
                // in the default format, which stays as narrow as it was.
                .value(
                    "cache_write",
                    session
                        .usage_cache_write()
                        .map_or_else(|| "-".to_string(), crate::timing::format_tokens),
                ),
        )
    }
}
