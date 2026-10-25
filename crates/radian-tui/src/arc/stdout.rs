//! A command worker's stdout as the task page's conversation tab draws it (ADR 0048 §13): the
//! attempt's `runs/<task>/attempts/<attempt>/result.jsonl`, one row per line, oldest first,
//! followed while the attempt runs.
//!
//! The format is the worker's, not radian's, so nothing here knows a provider's events: a line
//! that parses as a JSON object shows its `type` (or `-`) in the weak style and then one
//! summary, and everything else is the line itself. A summary is the first non-empty string
//! under [`SUMMARY_KEYS`], at the top level first and then one level down inside an
//! object-valued field (Codex's `item.text` and `item.command`). A `Result` row also shows its
//! `outcome` in the done or the failed state colour.
//!
//! The view reads incrementally: only the bytes appended since the last read, so a worker that
//! prints a lot never makes a 2-second refresh read the whole file. A line the worker is still
//! writing is a row too, and is replaced when it completes; [`poll`](StdoutView::poll) drops its
//! partial bytes when the file is truncated or replaced. Scrolling and following are the
//! attach view's: `j`/`k` pause the follow when they leave the end, `f` returns to it.

use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::path::PathBuf;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use serde_json::{Map, Value};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::theme::Theme;

/// The keys a row's summary is taken from, in this order (ADR 0048 §13).
pub const SUMMARY_KEYS: [&str; 5] = ["text", "message", "command", "content", "summary"];

/// One attempt's `result.jsonl`, read incrementally and drawn one row per line.
pub struct StdoutView {
    task: String,
    attempt: u32,
    program: String,
    path: PathBuf,
    /// Every complete line read so far, oldest first.
    lines: Vec<String>,
    /// The bytes after the newest newline: the line the worker is still writing. Shown as the
    /// last row and replaced when it completes.
    tail: Vec<u8>,
    /// How many bytes of the file are already in [`StdoutView::lines`] and [`StdoutView::tail`].
    offset: u64,
    /// The first row the last frame drew.
    scroll: usize,
    /// The last row the last frame could draw, from its measured height.
    max_scroll: usize,
    /// Whether new rows keep the end in view.
    follow: bool,
}

impl StdoutView {
    /// The view of one attempt's `result.jsonl`. The file may not exist yet; the first
    /// [`poll`](Self::poll) reads what is there.
    pub fn new(task: &str, attempt: u32, path: PathBuf, program: String) -> Self {
        Self {
            task: task.to_owned(),
            attempt,
            program,
            path,
            lines: Vec::new(),
            tail: Vec::new(),
            offset: 0,
            scroll: 0,
            max_scroll: 0,
            follow: true,
        }
    }

    /// The program the attempt's execution record names, for the tab's title.
    pub fn program(&self) -> &str {
        &self.program
    }

    /// Whether this view is the one for `(task, attempt)`.
    pub fn is_for(&self, task: &str, attempt: u32) -> bool {
        self.task == task && self.attempt == attempt
    }

    /// Replaces the title's program: the execution record may name it only once the attempt has
    /// ended.
    pub fn set_program(&mut self, program: String) {
        self.program = program;
    }

    /// Whether the view has no row at all.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty() && self.tail.is_empty()
    }

    /// Reads the bytes appended since the last read. A missing file is not an error: the worker
    /// may not have written anything yet. A file that shrank was truncated or replaced, so the
    /// rows start over from its new content.
    pub fn poll(&mut self) -> Result<bool, String> {
        let mut file = match std::fs::File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(format!("cannot read {}: {error}", self.path.display())),
        };
        let len = file
            .metadata()
            .map_err(|error| format!("cannot read {}: {error}", self.path.display()))?
            .len();
        if len < self.offset {
            self.lines.clear();
            self.tail.clear();
            self.offset = 0;
        }
        if len == self.offset {
            return Ok(false);
        }
        file.seek(SeekFrom::Start(self.offset))
            .map_err(|error| format!("cannot read {}: {error}", self.path.display()))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|error| format!("cannot read {}: {error}", self.path.display()))?;
        self.offset += bytes.len() as u64;
        let mut start = 0;
        for (index, byte) in bytes.iter().enumerate() {
            if *byte != b'\n' {
                continue;
            }
            let mut line = std::mem::take(&mut self.tail);
            line.extend_from_slice(&bytes[start..index]);
            self.lines.push(String::from_utf8_lossy(&line).into_owned());
            start = index + 1;
        }
        self.tail.extend_from_slice(&bytes[start..]);
        Ok(true)
    }

    /// One row: a complete line, or the line the worker is still writing.
    fn row_text(&self, index: usize) -> String {
        match self.lines.get(index) {
            Some(line) => line.clone(),
            None => String::from_utf8_lossy(&self.tail).into_owned(),
        }
    }

    /// Scrolls the rows one line at a time, up (`delta < 0`) or down. Leaving the end pauses
    /// the follow; returning to it resumes it, as the transcript's scroll does.
    pub fn scroll_lines(&mut self, delta: i32) {
        let step = delta.unsigned_abs() as usize;
        let next = if delta < 0 {
            self.scroll.saturating_sub(step)
        } else {
            self.scroll.saturating_add(step)
        };
        self.scroll = next.min(self.max_scroll);
        self.follow = self.scroll >= self.max_scroll;
    }

    /// Pins the view to its end (follow again).
    pub fn follow_end(&mut self) {
        self.scroll = self.max_scroll;
        self.follow = true;
    }

    /// Draws the visible rows into `area`, one row per line, each cut to the area's width. The
    /// frame's own rail is drawn by the caller, as it is for the attach view.
    pub fn draw(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let width = area.width as usize;
        let height = area.height as usize;
        let total = self.lines.len() + usize::from(!self.tail.is_empty());
        self.max_scroll = total.saturating_sub(height);
        self.scroll = if self.follow {
            self.max_scroll
        } else {
            self.scroll.min(self.max_scroll)
        };
        if height == 0 {
            return;
        }
        let end = (self.scroll + height).min(total);
        let mut lines = Vec::with_capacity(end.saturating_sub(self.scroll));
        for index in self.scroll..end {
            lines.push(row_line(&self.row_text(index), width, theme));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }
}

/// One stdout row: the JSON `type` in the weak style and then one summary, else the line itself.
fn row_line(raw: &str, width: usize, theme: &Theme) -> Line<'static> {
    let Ok(Value::Object(object)) = serde_json::from_str::<Value>(raw) else {
        return Line::from(spans_row(vec![(raw.to_owned(), theme.text)], width));
    };
    let kind = object.get("type").and_then(Value::as_str).unwrap_or("-");
    let mut parts = vec![(kind.to_owned(), theme.hint)];
    // A `Result` row's outcome is the state of the attempt: done when `completed`, failed for
    // every other value (ADR 0048 §13).
    if kind == "Result"
        && let Some(outcome) = object.get("outcome").and_then(Value::as_str)
    {
        let style = if outcome == "completed" {
            theme.status_ok
        } else {
            theme.status_error
        };
        parts.push((" ".to_owned(), theme.text));
        parts.push((outcome.to_owned(), style));
    }
    let body = match first_summary(&object) {
        Some(summary) => line_breaks_to_spaces(summary),
        None => Value::Object(object).to_string(),
    };
    parts.push((" ".to_owned(), theme.text));
    parts.push((body, theme.text));
    Line::from(spans_row(parts, width))
}

/// The first non-empty summary string: the keys of [`SUMMARY_KEYS`] at the top level first,
/// then the same keys one level down inside every object-valued field.
fn first_summary(object: &Map<String, Value>) -> Option<&str> {
    for key in SUMMARY_KEYS {
        if let Some(text) = object
            .get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            return Some(text);
        }
    }
    for value in object.values() {
        let Some(inner) = value.as_object() else {
            continue;
        };
        for key in SUMMARY_KEYS {
            if let Some(text) = inner
                .get(key)
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                return Some(text);
            }
        }
    }
    None
}

/// A summary is one row: its line breaks become spaces.
fn line_breaks_to_spaces(text: &str) -> String {
    text.replace("\r\n", " ").replace(['\n', '\r'], " ")
}

/// The styled parts of one row, cut to `width` cells. A row that is too long keeps the start of
/// every part and ends in `…`; a row that fits keeps every style.
fn spans_row(parts: Vec<(String, Style)>, width: usize) -> Vec<Span<'static>> {
    let total: usize = parts.iter().map(|(text, _)| text.width()).sum();
    if total <= width {
        return parts
            .into_iter()
            .map(|(text, style)| Span::styled(text, style))
            .collect();
    }
    if width == 0 {
        return Vec::new();
    }
    // One cell is the ellipsis's.
    let budget = width - 1;
    let mut out = Vec::new();
    let mut used = 0;
    'parts: for (text, style) in parts {
        let mut kept = String::new();
        for ch in text.chars() {
            let cell = UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + cell > budget {
                if !kept.is_empty() {
                    out.push(Span::styled(std::mem::take(&mut kept), style));
                }
                out.push(Span::styled("…", style));
                break 'parts;
            }
            kept.push(ch);
            used += cell;
        }
        if !kept.is_empty() {
            out.push(Span::styled(kept, style));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn a_summary_is_found_at_the_top_level_before_one_level_down() {
        let value: Value = serde_json::from_str(
            r#"{"type":"item.completed","message":"top","item":{"text":"nested"}}"#,
        )
        .unwrap();
        let Value::Object(object) = value else {
            panic!("an object");
        };
        assert_eq!(first_summary(&object), Some("top"));

        let value: Value =
            serde_json::from_str(r#"{"type":"item.completed","item":{"text":"nested"}}"#).unwrap();
        let Value::Object(object) = value else {
            panic!("an object");
        };
        assert_eq!(first_summary(&object), Some("nested"));
    }

    #[test]
    fn a_result_row_colors_its_outcome_and_a_non_object_is_the_line() {
        let theme = Theme::default();
        let done = row_line(
            r#"{"type":"Result","outcome":"completed","text":"all done"}"#,
            80,
            &theme,
        );
        assert_eq!(text(&done), "Result completed all done");
        let failed = row_line(
            r#"{"type":"Result","outcome":"error","text":"no"}"#,
            80,
            &theme,
        );
        assert_eq!(text(&failed), "Result error no");
        let plain = row_line("not json", 80, &theme);
        assert_eq!(text(&plain), "not json");
    }

    #[test]
    fn a_long_row_is_cut_with_an_ellipsis_and_fills_the_width() {
        let theme = Theme::default();
        let line = row_line(
            r#"{"type":"item.completed","item":{"text":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}"#,
            20,
            &theme,
        );
        let rendered = text(&line);
        assert_eq!(rendered.width(), 20);
        assert!(rendered.ends_with('…'), "{rendered:?}");
    }
}
