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
//! The worker's file is not radian's and has no size limit, so the view is bounded on purpose.
//! It reads incrementally, a chunk at a time and at most [`READ_CAP`] bytes per poll, so a
//! 2-second refresh never reads the whole file and never holds it. It keeps the newest
//! [`MAX_LINES`] complete rows and the line the worker is still writing, each line cut at
//! [`MAX_LINE_BYTES`]; a worker that prints without bound then costs a fixed amount of memory,
//! and scrolling reaches the rows of that window. A line the worker is still writing is a row
//! too, and is replaced when it completes; [`poll`](StdoutView::poll) drops its partial bytes
//! when the file is truncated or replaced. Scrolling and following are the attach view's:
//! `j`/`k` pause the follow when they leave the end, `f` returns to it.

use std::collections::VecDeque;
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

/// The most bytes one poll reads. A worker that writes faster than this falls behind by the
/// difference and the next refresh catches up; the view never reads or keeps the whole file.
const READ_CAP: u64 = 4 * 1024 * 1024;

/// The most complete lines the view keeps, newest last. The rows before this window are dropped:
/// the tab follows the end, so they are what a person can scroll through (ADR 0048 §13).
const MAX_LINES: usize = 4096;

/// The most bytes one line keeps. A row is cut to the terminal's width anyway, so the rest of a
/// very long line, or of one the worker has not terminated yet, is dropped.
const MAX_LINE_BYTES: usize = 8 * 1024;

/// One attempt's `result.jsonl`, read incrementally and drawn one row per line.
pub struct StdoutView {
    task: String,
    attempt: u32,
    program: String,
    path: PathBuf,
    /// The newest complete lines read, oldest first. Bounded by [`MAX_LINES`].
    lines: VecDeque<String>,
    /// The bytes after the newest newline: the line the worker is still writing. Shown as the
    /// last row and replaced when it completes. Bounded by [`MAX_LINE_BYTES`].
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
            lines: VecDeque::new(),
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

    /// Replaces the title's program: the wrapper's records may name it only once the attempt has
    /// started, and the execution record once it has ended.
    pub fn set_program(&mut self, program: String) {
        self.program = program;
    }

    /// Whether the view has no row at all.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty() && self.tail.is_empty()
    }

    /// Reads the bytes appended since the last read, in chunks and up to [`READ_CAP`] of them. A
    /// missing file is not an error: the worker may not have written anything yet. A file that
    /// shrank was truncated or replaced, so the rows start over from its new content.
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
        // The first read of a file larger than one poll starts at its end: the newest rows are
        // what the tab shows, and the line at that offset is a fragment whose start is gone.
        let start = if self.offset == 0 && len > READ_CAP {
            len - READ_CAP
        } else {
            self.offset
        };
        let skip_fragment = start > self.offset;
        file.seek(SeekFrom::Start(start))
            .map_err(|error| format!("cannot read {}: {error}", self.path.display()))?;
        self.offset = start;
        let mut remaining = len.saturating_sub(start).min(READ_CAP);
        // A fragment before the first newline is not a line. Kept aside only in case the whole
        // range is one unterminated line: then it is the newest line's prefix, and the tab shows
        // that rather than nothing.
        let mut fragment = skip_fragment.then(Vec::new);
        let mut buffer = [0u8; 64 * 1024];
        while remaining > 0 {
            let want = buffer.len().min(remaining as usize);
            let read = file
                .read(&mut buffer[..want])
                .map_err(|error| format!("cannot read {}: {error}", self.path.display()))?;
            if read == 0 {
                break;
            }
            self.offset += read as u64;
            remaining -= read as u64;
            for &byte in &buffer[..read] {
                if let Some(kept) = &mut fragment {
                    if byte == b'\n' {
                        fragment = None;
                    } else if kept.len() < MAX_LINE_BYTES {
                        kept.push(byte);
                    }
                    continue;
                }
                if byte == b'\n' {
                    let line = std::mem::take(&mut self.tail);
                    self.push_line(String::from_utf8_lossy(&line).into_owned());
                } else if self.tail.len() < MAX_LINE_BYTES {
                    self.tail.push(byte);
                }
            }
        }
        if let Some(kept) = fragment
            && !kept.is_empty()
        {
            self.tail = kept;
        }
        Ok(true)
    }

    /// Adds one complete line, dropping the oldest row past [`MAX_LINES`].
    fn push_line(&mut self, line: String) {
        if self.lines.len() == MAX_LINES {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
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
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    fn text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// A temp directory that removes itself.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir()
                .join(format!("radian-stdout-{label}-{}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("the temp dir is creatable");
            Self(dir)
        }

        fn file(&self) -> PathBuf {
            self.0.join("result.jsonl")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A view on an existing file, polled once.
    fn opened(path: &Path) -> StdoutView {
        let mut view = StdoutView::new("TK0001-task", 1, path.to_path_buf(), "codex".to_owned());
        view.poll().expect("the file is readable");
        view
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

    /// A worker can write a file far larger than the TUI may hold. The first poll starts at the
    /// file's end and reads [`READ_CAP`] bytes at most, a later burst is read across polls, and
    /// the view keeps the newest [`MAX_LINES`] rows; the older rows are gone, not materialized.
    ///
    /// Mutation: start every poll at `self.offset` (read the whole file from the start), keep
    /// every line, or read the whole appended range in one read.
    #[test]
    fn a_large_stdout_keeps_only_its_newest_rows() {
        let scratch = Scratch::new("large");
        let path = scratch.file();
        let total = READ_CAP as usize / 32 + 100;
        let mut content = String::new();
        for index in 0..total {
            content.push_str(&format!(
                "{{\"type\":\"item.completed\",\"item\":{{\"text\":\"row-{index:06}\"}}}}\n"
            ));
        }
        fs::write(&path, &content).unwrap();
        assert!(
            content.len() as u64 > READ_CAP,
            "the file is larger than one poll"
        );

        let mut view = opened(&path);
        assert_eq!(
            view.offset,
            content.len() as u64,
            "the first poll read from the end of the file"
        );
        assert_eq!(view.lines.len(), MAX_LINES, "the rows are bounded");
        assert_eq!(
            view.lines.back().unwrap(),
            &format!(
                "{{\"type\":\"item.completed\",\"item\":{{\"text\":\"row-{:06}\"}}}}",
                total - 1
            ),
            "the newest row is kept"
        );
        assert!(
            view.lines
                .front()
                .unwrap()
                .starts_with(r#"{"type":"item.completed""#),
            "the row at the window's start is a whole line: {:?}",
            view.lines.front()
        );

        // A worker that outruns one poll: the next poll reads at most [`READ_CAP`] and the one
        // after it catches up, so a burst never becomes one unbounded read.
        let before = view.offset;
        let mut extra = String::new();
        while (extra.len() as u64) < READ_CAP + 1024 {
            extra.push_str("{\"type\":\"item.completed\",\"item\":{\"text\":\"later\"}}\n");
        }
        {
            use std::io::Write as _;
            fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(extra.as_bytes())
                .unwrap();
        }
        view.poll().unwrap();
        assert_eq!(
            view.offset,
            before + READ_CAP,
            "one poll reads at most READ_CAP"
        );
        assert!(
            view.offset < path_len(&path),
            "the burst is not read in one poll"
        );
        view.poll().unwrap();
        assert_eq!(view.offset, path_len(&path), "the next poll catches up");
    }

    /// The first poll of a file larger than one poll starts inside a line; that fragment is not a
    /// row, so the view's first row is a whole line.
    #[test]
    fn the_first_poll_of_a_huge_file_starts_at_a_line_boundary() {
        let scratch = Scratch::new("boundary");
        let path = scratch.file();
        let line = "z".repeat(5000);
        let mut content = String::new();
        while (content.len() as u64) <= READ_CAP {
            content.push_str(&line);
            content.push('\n');
        }
        fs::write(&path, &content).unwrap();

        let view = opened(&path);
        assert_eq!(view.offset, content.len() as u64);
        assert!(!view.lines.is_empty());
        assert_eq!(
            view.lines.front().unwrap(),
            &line,
            "a whole line, not a fragment"
        );
        assert!(
            view.lines.iter().all(|row| row.len() == line.len()),
            "every row is a complete line"
        );
    }

    /// A line can be arbitrarily long, and the worker may never terminate it. The view keeps its
    /// first [`MAX_LINE_BYTES`] and drops the rest; the row is drawn cut to the width anyway.
    #[test]
    fn a_line_longer_than_the_cap_keeps_only_its_prefix() {
        let scratch = Scratch::new("long");
        let path = scratch.file();
        let long = "y".repeat(MAX_LINE_BYTES + 500);
        fs::write(&path, &long).unwrap();

        let mut view = opened(&path);
        assert_eq!(view.offset, long.len() as u64);
        assert_eq!(view.tail.len(), MAX_LINE_BYTES);
        assert!(!view.is_empty(), "the unterminated line is a row");
        let row = row_line(&view.row_text(0), 20, &Theme::default());
        assert_eq!(text(&row).width(), 20);
        assert!(text(&row).ends_with('…'));

        // It completes: the kept prefix becomes one bounded line, the rest is dropped.
        fs::write(&path, format!("{long}\nthe next line\n")).unwrap();
        view.poll().unwrap();
        assert_eq!(view.offset, path_len(&path));
        assert_eq!(view.lines.len(), 2);
        assert_eq!(view.lines[0].len(), MAX_LINE_BYTES);
        assert_eq!(view.lines[1], "the next line");
        assert!(view.tail.is_empty());
    }

    fn path_len(path: &Path) -> u64 {
        fs::metadata(path).unwrap().len()
    }
}
