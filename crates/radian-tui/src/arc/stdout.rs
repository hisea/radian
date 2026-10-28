//! A worker's stdout as the task page's conversation tab draws it (ADR 0048 §13): the
//! attempt's `runs/<task>/attempts/<attempt>/result.jsonl`, oldest first, followed while the
//! attempt runs.
//!
//! The format is the worker's, not radian's, so nothing here knows a provider's events. A
//! `command` worker's JSON lines are drawn one row per line: a JSON object shows its `type` (or
//! `-`) in the weak style and then one summary, and everything else is the line itself. A
//! summary is the first non-empty string under [`SUMMARY_KEYS`], at the top level first and then
//! one level down inside an object-valued field (Codex's `item.text` and `item.command`). A
//! `Result` row also shows its `outcome` in the done or the failed state colour.
//!
//! An ACP worker writes protocol lines (`{"type":"acp",...}`) instead, and those are folded
//! into the transcript the attach view draws ([`acp::AcpFold`]): the attempt's records name an
//! `acp` worker, or a `radian` worker whose session journal does not exist yet, or the first
//! lines the file gains are ACP. The fold is the same rendering path a resumed session uses —
//! Markdown, thinking blocks, tool cards and diffs — not a row per streamed token. A line that
//! is neither ACP nor `Result` still keeps the row drawing, so a mixed file stays readable. A
//! `command` worker that prints non-ACP JSONL keeps the row view unchanged.
//!
//! The worker's file is not radian's and has no size limit, so the view is bounded on purpose.
//! It reads incrementally, a chunk at a time and at most [`READ_CAP`] bytes per poll, so a
//! 2-second refresh never reads the whole file and never holds it. The row view keeps the newest
//! [`MAX_LINES`] complete rows, each stored line cut at [`MAX_LINE_BYTES`]; the line the worker
//! is still writing is held up to [`MAX_READ_LINE_BYTES`] so a diff is not cut before it can be
//! folded. The folded view keeps at most [`acp::MAX_ENTRIES`] entries and the newest
//! [`acp::MAX_ENTRY_BYTES`] of each one's text, appending instead of rebuilding; when the window
//! is full the oldest entries are dropped. Either way a worker that prints without bound costs
//! a fixed amount of memory, and scrolling reaches the kept window. A line the worker is still
//! writing is a row in the row view and is not folded until it is complete;
//! [`poll`](StdoutView::poll) drops its partial bytes when the file is truncated or replaced, and
//! resets the fold with it.
//! A truncation is a shorter file; a replacement is a file with another device or inode at the
//! same path, even when it is not shorter. A rewrite of the same file to the same length is not
//! observable and is read as no change. Scrolling and following are the attach view's: `j`/`k`
//! pause the follow when they leave the end, `f` returns to it.

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

mod acp;

use acp::AcpFold;

/// The keys a row's summary is taken from, in this order (ADR 0048 §13).
pub const SUMMARY_KEYS: [&str; 5] = ["text", "message", "command", "content", "summary"];

/// The most bytes one poll reads. A worker that writes faster than this falls behind by the
/// difference and the next refresh catches up; the view never reads or keeps the whole file.
const READ_CAP: u64 = 4 * 1024 * 1024;

/// The most complete lines the view keeps, newest last. The rows before this window are dropped:
/// the tab follows the end, so they are what a person can scroll through (ADR 0048 §13).
const MAX_LINES: usize = 4096;

/// The most bytes one row view line keeps. A row is cut to the terminal's width anyway, so the
/// rest of a very long line is dropped. The read buffer itself may hold a longer line (up to
/// [`READ_CAP`]) so an ACP `tool_call`'s diff is not cut before it is folded; a row is
/// truncated to this cap when it is stored.
const MAX_LINE_BYTES: usize = 8 * 1024;

/// The most bytes one unread line spends before it is cut. This is the poll's own cap: a line
/// longer than one poll cannot be read whole anyway. In the fold this bound keeps a single
/// huge diff from growing the read buffer past a poll.
const MAX_READ_LINE_BYTES: usize = READ_CAP as usize;

/// What the conversation renders: the row view, or a folded ACP transcript.
enum Mode {
    Rows,
    Acp(Box<AcpFold>),
}

/// One attempt's `result.jsonl`, read incrementally and drawn either one row per line (a command
/// worker) or as the folded ACP transcript (an ACP worker).
pub struct StdoutView {
    task: String,
    attempt: u32,
    program: String,
    path: PathBuf,
    /// The newest complete lines read, oldest first. Bounded by [`MAX_LINES`]. Used by the row
    /// view, and as the seed of the fold when the file turns out to be ACP.
    lines: VecDeque<String>,
    /// The bytes after the newest newline: the line the worker is still writing. Shown as the
    /// last row and replaced when it completes. Bounded by [`MAX_LINE_BYTES`].
    tail: Vec<u8>,
    /// How many bytes of the file are already in [`StdoutView::lines`] and [`StdoutView::tail`].
    offset: u64,
    /// The device and inode of the file the offset belongs to, while it is known. A different
    /// file at the same path is a replacement even when its length is not smaller.
    identity: Option<(u64, u64)>,
    /// What the conversation draws.
    mode: Mode,
    /// The first row the last frame drew (the row view).
    scroll: usize,
    /// The last row the last frame could draw, from its measured height (the row view).
    max_scroll: usize,
    /// Whether new rows keep the end in view (the row view).
    follow: bool,
}

impl StdoutView {
    /// The view of one attempt's `result.jsonl`. The file may not exist yet; the first
    /// [`poll`](Self::poll) reads what is there. `acp` says the attempt's record names an ACP
    /// worker; a `command` worker starts on the row view and switches when its lines are ACP.
    pub fn new(task: &str, attempt: u32, path: PathBuf, program: String, acp: bool) -> Self {
        Self {
            task: task.to_owned(),
            attempt,
            program,
            path,
            lines: VecDeque::new(),
            tail: Vec::new(),
            offset: 0,
            identity: None,
            mode: if acp {
                Mode::Acp(Box::new(AcpFold::new()))
            } else {
                Mode::Rows
            },
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
        match &self.mode {
            // A partial line is not folded until it is complete, so it is not a row either.
            Mode::Acp(fold) => fold.is_empty(),
            Mode::Rows => self.lines.is_empty() && self.tail.is_empty(),
        }
    }

    /// Reads the bytes appended since the last read, in chunks and up to [`READ_CAP`] of them. A
    /// missing file is not an error: the worker may not have written anything yet. A shorter file
    /// was truncated; a file with another device or inode is a replacement. Either way the rows
    /// start over from its new content. A rewrite of the same file to the same length cannot be
    /// told apart from an append and is read from the old offset.
    pub fn poll(&mut self) -> Result<bool, String> {
        let mut file = match std::fs::File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(format!("cannot read {}: {error}", self.path.display())),
        };
        let metadata = file
            .metadata()
            .map_err(|error| format!("cannot read {}: {error}", self.path.display()))?;
        let len = metadata.len();
        #[cfg(unix)]
        let identity = {
            use std::os::unix::fs::MetadataExt as _;
            (metadata.dev(), metadata.ino())
        };
        #[cfg(not(unix))]
        let identity = (0, 0);
        let replaced = self.identity.is_some_and(|known| known != identity);
        if replaced || len < self.offset {
            self.lines.clear();
            self.tail.clear();
            self.offset = 0;
            if let Mode::Acp(_) = &self.mode {
                self.mode = Mode::Acp(Box::new(AcpFold::new()));
            }
        }
        self.identity = Some(identity);
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
                    } else if kept.len() < MAX_READ_LINE_BYTES {
                        kept.push(byte);
                    }
                    continue;
                }
                if byte == b'\n' {
                    let line = std::mem::take(&mut self.tail);
                    self.push_line(String::from_utf8_lossy(&line).into_owned());
                } else if self.tail.len() < MAX_READ_LINE_BYTES {
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

    /// Adds one complete line: to the row view, or to the folded transcript. The first ACP line
    /// of a row view switches it to the fold, and the rows read before it are seeded into the
    /// fold in order (ADR 0048 §13).
    fn push_line(&mut self, line: String) {
        match &mut self.mode {
            Mode::Rows => {
                if is_acp_line(&line) {
                    self.enter_acp(&line);
                } else {
                    if self.lines.len() == MAX_LINES {
                        self.lines.pop_front();
                    }
                    self.lines.push_back(truncate_line(line));
                }
            }
            Mode::Acp(fold) => fold.push_line(&line),
        }
    }

    /// Switches a row view to the fold: every row already read becomes the fold's first entries,
    /// then `line` is folded.
    fn enter_acp(&mut self, line: &str) {
        let rows = std::mem::take(&mut self.lines);
        let mut fold = AcpFold::new();
        for row in rows {
            fold.push_line(&row);
        }
        fold.push_line(line);
        self.mode = Mode::Acp(Box::new(fold));
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
        if let Mode::Acp(fold) = &mut self.mode {
            fold.scroll_lines(delta);
            return;
        }
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
        if let Mode::Acp(fold) = &mut self.mode {
            fold.follow_end();
            return;
        }
        self.scroll = self.max_scroll;
        self.follow = true;
    }

    /// Draws the visible rows into `area`, one row per line, each cut to the area's width (or the
    /// folded ACP transcript through the attach view's renderer). The frame's own rail is drawn
    /// by the caller, as it is for the attach view.
    pub fn draw(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        if let Mode::Acp(fold) = &mut self.mode {
            fold.draw(frame, area, theme);
            return;
        }
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

/// Whether one complete line is an ACP envelope (`{"type":"acp",...}`). A row view that meets
/// one switches to the fold, so a command worker that happens to print the protocol is drawn as
/// a transcript too (ADR 0048 §13).
fn is_acp_line(raw: &str) -> bool {
    serde_json::from_str::<Value>(raw)
        .is_ok_and(|value| value.get("type").and_then(Value::as_str) == Some("acp"))
}

/// Cuts a row view line to [`MAX_LINE_BYTES`] at a character boundary. The read buffer holds the
/// whole line (an ACP line may carry a diff); a stored row keeps the row view's own bound.
fn truncate_line(mut line: String) -> String {
    if line.len() <= MAX_LINE_BYTES {
        return line;
    }
    let mut end = MAX_LINE_BYTES;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    line.truncate(end);
    line
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

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

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

    /// A view on an existing file, polled once. The row view unless `acp` asks for the fold.
    fn opened_with(path: &Path, acp: bool) -> StdoutView {
        let mut view = StdoutView::new(
            "TK0001-task",
            1,
            path.to_path_buf(),
            "codex".to_owned(),
            acp,
        );
        view.poll().expect("the file is readable");
        view
    }

    fn opened(path: &Path) -> StdoutView {
        opened_with(path, false)
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

    /// A command worker's non-ACP rows keep the row view; the first ACP line switches the view
    /// to the fold and seeds the rows read before it (ADR 0048 §13).
    #[test]
    fn an_acp_line_switches_a_row_view_to_the_fold() {
        let scratch = Scratch::new("switch");
        let path = scratch.file();
        let acp = serde_json::json!({
            "type": "acp",
            "message": {
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": "s",
                    "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "content": {"type": "text", "text": "the folded answer"},
                    },
                },
            },
        })
        .to_string();
        fs::write(
            &path,
            [
                r#"{"type":"item.completed","item":{"text":"the first row"}}"#.to_owned(),
                acp,
            ]
            .join("\n")
                + "\n",
        )
        .unwrap();
        let view = opened(&path);
        let Mode::Acp(fold) = &view.mode else {
            panic!("the view folded the ACP stream");
        };
        assert_eq!(fold.app.session.entries.len(), 2);
        assert_eq!(
            fold.app.session.entries[0].kind,
            crate::app::EntryKind::Notice
        );
        assert_eq!(
            fold.app.session.entries[0].text,
            "item.completed the first row"
        );
        assert_eq!(
            fold.app.session.entries[1].kind,
            crate::app::EntryKind::Assistant
        );
        assert_eq!(fold.app.session.entries[1].text, "the folded answer");
    }

    /// A view whose attempt record names an ACP worker starts folded, so the first line is a
    /// transcript block rather than one row (ADR 0048 §13).
    #[test]
    fn a_record_naming_an_acp_worker_starts_folded() {
        let scratch = Scratch::new("acp-start");
        let path = scratch.file();
        let acp = serde_json::json!({
            "type": "acp",
            "message": {
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": "s",
                    "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "content": {"type": "text", "text": "the answer"},
                    },
                },
            },
        })
        .to_string();
        fs::write(&path, format!("{acp}\n")).unwrap();
        let view = opened_with(&path, true);
        let Mode::Acp(fold) = &view.mode else {
            panic!("the view starts folded");
        };
        assert_eq!(fold.app.session.entries.last().unwrap().text, "the answer");
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

    /// A line can be arbitrarily long, and the worker may never terminate it. The read buffer
    /// holds it up to the poll's own [`MAX_READ_LINE_BYTES`] (an ACP line may carry a diff), and
    /// a stored row keeps only its first [`MAX_LINE_BYTES`]; the row is drawn cut to the width
    /// anyway.
    #[test]
    fn a_line_longer_than_the_cap_keeps_only_its_prefix() {
        let scratch = Scratch::new("long");
        let path = scratch.file();
        let long = "y".repeat(MAX_LINE_BYTES + 500);
        fs::write(&path, &long).unwrap();

        let mut view = opened(&path);
        assert_eq!(view.offset, long.len() as u64);
        assert_eq!(
            view.tail.len(),
            long.len(),
            "the unterminated line is held for the fold"
        );
        assert!(view.tail.len() <= MAX_READ_LINE_BYTES);
        assert!(!view.is_empty(), "the unterminated line is a row");
        let row = row_line(&view.row_text(0), 20, &Theme::default());
        assert_eq!(text(&row).width(), 20);
        assert!(text(&row).ends_with('…'));

        // It completes: the kept prefix becomes one bounded row, the rest is dropped.
        fs::write(&path, format!("{long}\nthe next line\n")).unwrap();
        view.poll().unwrap();
        assert_eq!(view.offset, path_len(&path));
        assert_eq!(view.lines.len(), 2);
        assert_eq!(view.lines[0].len(), MAX_LINE_BYTES);
        assert_eq!(view.lines[1], "the next line");
        assert!(view.tail.is_empty());
    }

    /// A file replaced at the same path starts over even when its length is not smaller: the
    /// view compares the device and inode, not only the length.
    ///
    /// Mutation: compare only `len < offset` and the replaced bytes are read as an append (or
    /// not at all when the lengths are equal).
    #[test]
    fn a_replaced_file_starts_over_when_it_is_not_shorter() {
        let scratch = Scratch::new("replace");
        let path = scratch.file();
        let first = "first line\n";
        fs::write(&path, first).unwrap();
        let mut view = opened(&path);
        assert_eq!(view.lines.len(), 1);
        assert_eq!(view.offset, first.len() as u64);

        // A different file of the same length, moved over the old one: a rename gives it
        // another inode, and the length alone would see no change at all.
        let replacement = "other line\n";
        assert_eq!(replacement.len(), first.len());
        let new_path = scratch.0.join("new.jsonl");
        fs::write(&new_path, replacement).unwrap();
        fs::rename(&new_path, &path).unwrap();

        assert!(view.poll().unwrap(), "the replacement is read");
        assert_eq!(view.lines.len(), 1);
        assert_eq!(view.lines[0], "other line");
        assert_eq!(view.offset, replacement.len() as u64);
    }

    /// ACP lines are not cut at the row view's 8 KiB before they are parsed: a tool call's diff
    /// can be wider than a row, and the fold then caps the entry's own text (ADR 0048 §13).
    #[test]
    fn a_long_acp_line_is_folded_whole() {
        let scratch = Scratch::new("acp-long-line");
        let path = scratch.file();
        let old = "a".repeat(12 * 1024);
        let new = "b".repeat(12 * 1024);
        let line = serde_json::json!({
            "type": "acp",
            "message": {
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": "s",
                    "update": {
                        "sessionUpdate": "tool_call",
                        "toolCallId": "c1",
                        "title": "Edit a large file",
                        "status": "completed",
                        "content": [{"type": "diff", "path": "big.txt", "oldText": old, "newText": new}],
                    },
                },
            },
        })
        .to_string();
        assert!(line.len() > MAX_LINE_BYTES, "the line is wider than a row");
        fs::write(&path, format!("{line}\n")).unwrap();
        let view = opened_with(&path, true);
        let Mode::Acp(fold) = &view.mode else {
            panic!("the view is folded");
        };
        assert_eq!(fold.app.session.entries.len(), 2);
        assert_eq!(fold.app.session.entries[0].text, "Edit a large file");
        let result = &fold.app.session.entries[1];
        assert!(
            !result.text.is_empty() && result.text.len() <= 8 * 1024,
            "the diff body is capped: {}",
            result.text.len()
        );
    }

    fn path_len(path: &Path) -> u64 {
        fs::metadata(path).unwrap().len()
    }

    fn acp_chunk(text: &str) -> String {
        serde_json::json!({
            "type": "acp",
            "message": {
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": "s",
                    "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "content": {"type": "text", "text": text},
                    },
                },
            },
        })
        .to_string()
    }

    /// A truncated or replaced file resets the fold, as the row view resets its rows: the next
    /// content starts from an empty transcript (ADR 0048 §13).
    #[test]
    fn a_truncated_acp_file_resets_the_fold() {
        let scratch = Scratch::new("acp-reset");
        let path = scratch.file();
        fs::write(
            &path,
            format!("{}\n{}\n", acp_chunk("the old answer"), acp_chunk(" more")),
        )
        .unwrap();
        let mut view = opened_with(&path, true);
        let Mode::Acp(fold) = &view.mode else {
            panic!("the view is folded");
        };
        assert_eq!(fold.app.session.entries.len(), 1);
        assert_eq!(fold.app.session.entries[0].text, "the old answer more");

        // A shorter file is a truncation: the fold starts over from its content.
        let fresh = acp_chunk("fresh");
        fs::write(&path, format!("{fresh}\n")).unwrap();
        assert!(view.poll().unwrap());
        let Mode::Acp(fold) = &view.mode else {
            panic!("the view stays folded");
        };
        assert_eq!(fold.app.session.entries.len(), 1);
        assert_eq!(fold.app.session.entries[0].text, "fresh");
    }

    /// A codex-like ACP stream of one-token Chinese chunks draws as one wrapped paragraph
    /// through the attach renderer, not one row per chunk; the hidden `usage_update` is a weak
    /// status line, never raw JSON (ADR 0048 §13).
    #[test]
    fn a_codex_like_stream_draws_as_one_paragraph_not_one_row_per_token() {
        let scratch = Scratch::new("acp-render");
        let path = scratch.file();
        let mut content = String::new();
        for _ in 0..200 {
            content.push_str(&acp_chunk("你"));
            content.push('\n');
        }
        content.push_str(
            &serde_json::json!({
                "type": "acp",
                "message": {
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": "s",
                        "update": {"sessionUpdate": "usage_update", "used": 1, "size": 9000},
                    },
                },
            })
            .to_string(),
        );
        content.push('\n');
        content.push_str(r#"{"type":"Result","outcome":"completed","text":"done"}"#);
        content.push('\n');
        fs::write(&path, content).unwrap();

        let mut view = opened_with(&path, true);
        let mut terminal = Terminal::new(TestBackend::new(40, 40)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                view.draw(frame, area, &Theme::default());
            })
            .unwrap();
        let screen = terminal.backend().to_string();
        assert_eq!(
            screen.matches('你').count(),
            200,
            "the whole answer is drawn:\n{screen}"
        );
        let rows = screen.lines().filter(|row| row.contains('你')).count();
        assert!(
            (2..=20).contains(&rows),
            "the chunks wrap into a paragraph, not 200 rows: {rows}\n{screen}"
        );
        assert!(
            screen.contains("context 1/9000"),
            "the usage is one weak line:\n{screen}"
        );
        assert!(
            !screen.contains("usage_update") && !screen.contains("\"used\""),
            "no usage JSON is dumped:\n{screen}"
        );
        assert!(
            screen.contains("Result completed done"),
            "the result line is drawn:\n{screen}"
        );
    }
}
