//! Reading `.radian/arc/loop.log` for the TUI's ticker and its `L` view (ADR 0048 decision
//! 13). The runner (`cli/arc.rs`) reads the file and hands the result to [`ArcApp`]; the page
//! state itself reads no file.
//!
//! The ticker shows the newest line the file gained after the TUI opened, when that line is a
//! transition line rather than part of a summary block. The log view shows the last
//! [`MAX_LINES`] lines and follows new ones while it is scrolled to the bottom.

use std::path::PathBuf;

use radian_arc::run::LOOP_LOG;
use radian_arc::store::Store;

/// The most lines the log view holds: the last 500 of `loop.log`.
pub const MAX_LINES: usize = 500;

/// One read of the log: the tail the view draws, and a ticker line when the file gained one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopLogRead {
    /// The last [`MAX_LINES`] lines, timestamps included.
    pub lines: Vec<String>,
    /// The newest transition line appended since the last read, without its timestamp.
    pub ticker: Option<String>,
}

/// The tail of one repository's `loop.log`, with what the previous read saw.
pub struct LoopLogTail {
    path: PathBuf,
    /// How many lines the last read saw, so only lines appended since reach the ticker.
    seen: usize,
    /// Whether a read has happened: the file's content at open is not news.
    started: bool,
}

impl LoopLogTail {
    /// The tail reader of `store`'s repository.
    #[must_use]
    pub fn new(store: &Store) -> Self {
        Self {
            path: store.root().join(LOOP_LOG),
            seen: 0,
            started: false,
        }
    }

    /// Reads the file. `None` when `loop.log` does not exist (the log view says so); the first
    /// read after the TUI opened sets the baseline, and a file that appears later ticks.
    pub fn read(&mut self) -> Option<LoopLogRead> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(_) => {
                // A missing log at open is the baseline: the first line a loop writes later is
                // news.
                self.started = true;
                self.seen = 0;
                return None;
            }
        };
        let total = text.lines().count();
        let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
        if lines.len() > MAX_LINES {
            lines.drain(..lines.len() - MAX_LINES);
        }
        // A rotation starts a shorter file: every line in it is new.
        let new_lines = if total < self.seen {
            total
        } else {
            total - self.seen
        };
        let ticker = if self.started && new_lines > 0 {
            let from = lines.len().saturating_sub(new_lines);
            lines[from..]
                .iter()
                .rev()
                .find_map(|line| transition_line(line))
        } else {
            None
        };
        self.started = true;
        self.seen = total;
        Some(LoopLogRead { lines, ticker })
    }
}

/// The last `radian arc:` line of `loop.log`, without its timestamp: what a failed start names
/// after its exit code.
#[must_use]
pub fn last_error_line(store: &Store) -> Option<String> {
    let text = std::fs::read_to_string(store.root().join(LOOP_LOG)).ok()?;
    text.lines()
        .rev()
        .map(content)
        .find(|line| line.starts_with("radian arc:"))
        .map(str::to_owned)
}

/// The line without the `YYYY-MM-DDTHH:MM:SS.mmmZ ` prefix the loop writes.
fn content(line: &str) -> &str {
    match line.split_once(' ') {
        Some((stamp, rest)) if stamp.len() == 24 && stamp.contains('T') && stamp.ends_with('Z') => {
            rest
        }
        _ => line,
    }
}

/// A line the ticker may show: a transition line, not a summary block line. The summary starts
/// at `summary` (or `no transition`), its notes and waiting rows are indented, and its plan
/// headers are `<id> (auto <level>)` or `<id> (changed outside arc; …)`. Everything else that
/// is unindented is a transition.
fn transition_line(line: &str) -> Option<String> {
    let text = content(line);
    if text.is_empty()
        || text.starts_with(' ')
        || matches!(text, "summary" | "no transition" | "nothing waits")
        || text.contains(" (auto ")
        || text.contains(" (changed outside arc")
    {
        return None;
    }
    Some(text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timestamp_is_stripped_and_summary_lines_do_not_tick() {
        assert_eq!(
            content("2026-10-24T10:00:00.000Z TK0001-work-1 started -> doing.running"),
            "TK0001-work-1 started -> doing.running"
        );
        assert_eq!(
            transition_line("2026-10-24T10:00:00.000Z TK0001-work-1 started -> doing.running")
                .as_deref(),
            Some("TK0001-work-1 started -> doing.running")
        );
        assert_eq!(transition_line("2026-10-24T10:00:00.000Z summary"), None);
        assert_eq!(
            transition_line("2026-10-24T10:00:00.000Z no transition"),
            None
        );
        assert_eq!(
            transition_line("2026-10-24T10:00:00.000Z   TK0001-work-1 todo.ready: waits"),
            None
        );
        assert_eq!(
            transition_line("2026-10-24T10:00:00.000Z PL0001-loop (auto run)"),
            None
        );
        assert_eq!(
            transition_line(
                "2026-10-24T10:00:00.000Z PL0001 (changed outside arc; run `radian arc adopt PL0001`)"
            ),
            None
        );
        assert_eq!(
            transition_line("2026-10-24T10:00:00.000Z nothing waits"),
            None
        );
        // A line without the stamp is still read (an older file, or a test's own line).
        assert_eq!(content("plain line"), "plain line");
    }
}
