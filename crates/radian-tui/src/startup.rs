//! The startup screen: the wordmark, a collapsed information line, the
//! safety lines that are never folded away, and one Tip. It is the leading block of
//! [`crate::app::App::entries`], so the transcript's wrap, scroll and selection machinery
//! treat it like any other content.
//!
//! The block is rebuilt when it is toggled (`Ctrl-P`) or when the terminal width crosses the
//! wordmark's threshold; `App` owns that (see `App::rebuild_startup_entries`).

use unicode_width::UnicodeWidthStr;

/// The wordmark, three rows, exactly as the owner's ADR 0027 §标志 chose. Any terminal font
/// can draw these box-drawing characters. The right column (version / model / path) is
/// appended at build time.
pub const BANNER_WIDE: [&str; 3] = [
    "╭─╮ ╭─╮ ┬─╮ ╷ ╭─╮ ┬╮╷",
    "├┬╯ ├─┤ │ │ │ ├─┤ │││",
    "╵╰╴ ╵ ╵ ┴─╯ ╵ ╵ ╵ ╵╰┴",
];

// ADR 0027: the one-line fallback's brand mark comes from the footer's glyph set
// (`radian_tui::footer::brand_mark`), not a fixed `㎭`.

/// The gap between the wordmark and its right column.
const BANNER_GAP: &str = "   ";

/// The built-in tips. Every one names a feature that exists today; `app::tests::
/// every_tip_names_a_real_feature` fails when one drifts.
pub const TIPS: &[&str] = &[
    "Ctrl-F searches the transcript",
    "Ctrl-C twice quits",
    "/theme switches the color theme",
    "/compact summarizes the history now",
    "@path attaches a file to the request",
    "Shift+drag uses the terminal's own selection",
    "!<command> runs a shell command in the sandbox",
];

/// The tip for a session: a stable hash of the session id picks one, so the same session
/// shows the same tip on resume. No randomness.
pub fn tip_for(session_id: &str) -> &'static str {
    // FNV-1a, so the choice is fixed for a given id across builds.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in session_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    TIPS[(hash % TIPS.len() as u64) as usize]
}

/// The startup screen's data, rendered as the leading entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupScreen {
    /// The wordmark rows, right column appended (empty when `tui.banner = false`).
    pub banner: Vec<String>,
    /// The one-line fallback the wordmark drops to when the terminal is too narrow.
    pub banner_narrow: String,
    /// The collapsed summary (`workspace · macOS sandbox · context …`).
    pub summary: String,
    /// The full lines, shown when expanded.
    pub lines: Vec<String>,
    /// The safety lines: always visible, never folded.
    pub safety: Vec<String>,
    /// The Tip body (without the `Tip: ` prefix), or `None` when `tui.tips = false`.
    pub tip: Option<String>,
    /// Whether the information block is expanded.
    pub expanded: bool,
}

impl StartupScreen {
    /// The banner rows for `width`: the three-row wordmark when every row fits, otherwise
    /// the one-line `<brand> radian <version>` fallback.
    pub fn banner_for_width(&self, width: usize) -> Vec<String> {
        if self.banner.is_empty() {
            return Vec::new();
        }
        let widest = self.banner.iter().map(|row| row.width()).max().unwrap_or(0);
        if width >= widest {
            self.banner.clone()
        } else {
            vec![self.banner_narrow.clone()]
        }
    }
}

/// Builds the wordmark rows: the three fixed rows with the right column (version, model,
/// path) starting at a fixed column after the wordmark.
pub fn banner_rows(version: &str, model: &str, path: &str) -> Vec<String> {
    let column = BANNER_WIDE.iter().map(|row| row.width()).max().unwrap_or(0) + BANNER_GAP.width();
    BANNER_WIDE
        .iter()
        .zip([version, model, path])
        .map(|(left, right)| {
            let pad = " ".repeat(column.saturating_sub(left.width()));
            format!("{left}{pad}{right}")
        })
        .collect()
}
