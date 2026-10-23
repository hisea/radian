//! The input composer (ADR 0009 Phase 4): the text plus a cursor, with the editing keys a
//! terminal coding agent is expected to have. It is pure data (no terminal I/O), so every
//! operation is unit-testable.
//!
//! Positions are counted in `char`s, never bytes, so a multi-byte character can never be
//! split. The cursor's on-screen cell uses `unicode-width`, so a CJK character or an emoji
//! occupies its real width.

use std::ops::Range;
use std::path::{Path, PathBuf};

use unicode_width::UnicodeWidthChar;

/// A text buffer with a cursor. `cursor` is a `char` index in `0..=chars.len()`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Composer {
    chars: Vec<char>,
    cursor: usize,
}

impl Composer {
    pub fn new() -> Self {
        Self::default()
    }

    /// A composer holding `text`, with the cursor at the end.
    pub fn from_text(text: &str) -> Self {
        let mut composer = Self::new();
        composer.set_text(text);
        composer
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    /// Replaces the text and puts the cursor at the end.
    pub fn set_text(&mut self, text: &str) {
        self.chars = text.chars().collect();
        self.cursor = self.chars.len();
    }

    pub fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    /// The number of `char`s.
    pub fn len(&self) -> usize {
        self.chars.len()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Moves the cursor to the end of the buffer (used when the `Other` row is chosen, so the
    /// next keys type at its end).
    pub fn to_end(&mut self) {
        self.cursor = self.chars.len();
    }

    /// Inserts one character at the cursor.
    pub fn insert_char(&mut self, c: char) {
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
    }

    /// Inserts a string at the cursor (bracketed paste). `\r\n` and a lone `\r` are normalized
    /// to `\n`; nothing else is changed, and nothing is sent.
    pub fn insert_str(&mut self, text: &str) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        let inserted: Vec<char> = normalized.chars().collect();
        let count = inserted.len();
        self.chars.splice(self.cursor..self.cursor, inserted);
        self.cursor += count;
    }

    /// Removes the chars in `range` (a dropped path that became an attachment). The cursor
    /// moves to the range's start, clamped to the buffer.
    pub fn remove_range(&mut self, range: Range<usize>) {
        let start = range.start.min(self.chars.len());
        let end = range.end.min(self.chars.len()).max(start);
        self.chars.drain(start..end);
        self.cursor = if self.cursor <= start {
            self.cursor
        } else if self.cursor >= end {
            self.cursor - (end - start)
        } else {
            start
        };
    }

    pub fn left(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor -= 1;
        true
    }

    pub fn right(&mut self) -> bool {
        if self.cursor >= self.chars.len() {
            return false;
        }
        self.cursor += 1;
        true
    }

    /// Moves to the start of the previous word (whitespace-delimited).
    pub fn word_left(&mut self) -> bool {
        let start = self.cursor;
        let mut i = self.cursor;
        while i > 0 && self.chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !self.chars[i - 1].is_whitespace() {
            i -= 1;
        }
        self.cursor = i;
        i != start
    }

    /// Moves past the next word (whitespace-delimited).
    pub fn word_right(&mut self) -> bool {
        let start = self.cursor;
        let mut i = self.cursor;
        while i < self.chars.len() && self.chars[i].is_whitespace() {
            i += 1;
        }
        while i < self.chars.len() && !self.chars[i].is_whitespace() {
            i += 1;
        }
        self.cursor = i;
        i != start
    }

    /// The start of the cursor's logical line (Ctrl-A / Home).
    pub fn line_start(&mut self) -> bool {
        let start = self.line_start_at(self.cursor);
        let moved = start != self.cursor;
        self.cursor = start;
        moved
    }

    /// The end of the cursor's logical line (Ctrl-E / End).
    pub fn line_end(&mut self) -> bool {
        let end = self.line_end_at(self.cursor);
        let moved = end != self.cursor;
        self.cursor = end;
        moved
    }

    /// Deletes the character before the cursor.
    pub fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.chars.remove(self.cursor - 1);
        self.cursor -= 1;
        true
    }

    /// Deletes the character under the cursor.
    pub fn delete(&mut self) -> bool {
        if self.cursor >= self.chars.len() {
            return false;
        }
        self.chars.remove(self.cursor);
        true
    }

    /// Deletes back to the start of the previous word (Ctrl-W).
    pub fn delete_word(&mut self) -> bool {
        let start = self.cursor;
        let mut i = self.cursor;
        while i > 0 && self.chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !self.chars[i - 1].is_whitespace() {
            i -= 1;
        }
        if i == start {
            return false;
        }
        self.chars.drain(i..start);
        self.cursor = i;
        true
    }

    /// Deletes to the start of the logical line (Ctrl-U).
    pub fn kill_to_line_start(&mut self) -> bool {
        let start = self.line_start_at(self.cursor);
        if start == self.cursor {
            return false;
        }
        self.chars.drain(start..self.cursor);
        self.cursor = start;
        true
    }

    /// Deletes to the end of the logical line (Ctrl-K).
    pub fn kill_to_line_end(&mut self) -> bool {
        let end = self.line_end_at(self.cursor);
        if end == self.cursor {
            return false;
        }
        self.chars.drain(self.cursor..end);
        true
    }

    /// Moves the cursor up one logical line, keeping the column where possible. False when
    /// already on the first line.
    pub fn up(&mut self) -> bool {
        let start = self.line_start_at(self.cursor);
        if start == 0 {
            return false;
        }
        let column = self.cursor - start;
        let previous_end = start - 1; // the '\n'
        let previous_start = self.line_start_at(previous_end);
        self.cursor = (previous_start + column).min(previous_end);
        true
    }

    /// Moves the cursor down one logical line, keeping the column where possible. False when
    /// already on the last line.
    pub fn down(&mut self) -> bool {
        let end = self.line_end_at(self.cursor);
        if end >= self.chars.len() {
            return false;
        }
        let column = self.cursor - self.line_start_at(self.cursor);
        let next_start = end + 1;
        let next_end = self.line_end_at(next_start);
        self.cursor = (next_start + column).min(next_end);
        true
    }

    /// True when the cursor is on the first logical line (so `Up` means "history").
    pub fn is_first_line(&self) -> bool {
        !self.chars[..self.cursor].contains(&'\n')
    }

    /// True when the cursor is on the last logical line (so `Down` means "history").
    pub fn is_last_line(&self) -> bool {
        !self.chars[self.cursor..].contains(&'\n')
    }

    /// The cursor's cell `(row, display column)` when the text is wrapped to `width` columns
    /// by [`rows`]. Wide characters count their real width, so a CJK line's cursor is exact.
    pub fn cursor_cell(&self, width: usize) -> (usize, usize) {
        let width = width.max(1);
        let mut row = 0usize;
        let mut col = 0usize;
        for (i, ch) in self.chars.iter().enumerate() {
            if i == self.cursor {
                break;
            }
            if *ch == '\n' {
                row += 1;
                col = 0;
                continue;
            }
            let cell = ch.width().unwrap_or(0);
            if col + cell > width && col > 0 {
                row += 1;
                col = 0;
            }
            col += cell;
        }
        (row, col)
    }

    fn line_start_at(&self, mut i: usize) -> usize {
        while i > 0 && self.chars[i - 1] != '\n' {
            i -= 1;
        }
        i
    }

    fn line_end_at(&self, mut i: usize) -> usize {
        while i < self.chars.len() && self.chars[i] != '\n' {
            i += 1;
        }
        i
    }
}

impl From<&str> for Composer {
    fn from(text: &str) -> Self {
        Composer::from_text(text)
    }
}

impl From<String> for Composer {
    fn from(text: String) -> Self {
        Composer::from_text(&text)
    }
}

impl std::fmt::Display for Composer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for ch in &self.chars {
            f.write_str(&ch.to_string())?;
        }
        Ok(())
    }
}

impl PartialEq<str> for Composer {
    fn eq(&self, other: &str) -> bool {
        self.chars.iter().copied().eq(other.chars())
    }
}

impl PartialEq<&str> for Composer {
    fn eq(&self, other: &&str) -> bool {
        self == *other
    }
}

/// Hard-wraps `text` to `width` display columns, preserving every character (a `\n` always
/// starts a new row). Unlike the transcript's `wrap`, spaces are not collapsed: the input box
/// shows exactly what will be sent.
pub fn rows(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for logical in text.split('\n') {
        let mut line = String::new();
        let mut line_width = 0usize;
        for ch in logical.chars() {
            let cell = ch.width().unwrap_or(0);
            if line_width + cell > width && line_width > 0 {
                out.push(std::mem::take(&mut line));
                line_width = 0;
            }
            line.push(ch);
            line_width += cell;
        }
        out.push(line);
    }
    out
}

/// The draft file for one session, next to its `.jsonl`. The session directory is
/// sandbox-denied (ADR 0020), so the draft is as unreadable to a sandboxed command as the
/// session itself.
pub fn draft_path(session_dir: &Path, session_id: &str) -> PathBuf {
    session_dir.join(format!("{session_id}.draft"))
}

/// Writes the draft atomically (temp file + rename), owner-only (0600). Empty text removes any
/// existing draft rather than writing an empty file.
pub fn save_draft(path: &Path, text: &str) -> std::io::Result<()> {
    if text.is_empty() {
        return remove_draft(path);
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("draft");
    let temp = path.with_file_name(format!(".{name}.tmp-{}", std::process::id()));
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        use std::io::Write;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&temp, path)
}

/// Reads the draft and removes it (a resume restores it once). `None` when there is none.
pub fn take_draft(path: &Path) -> std::io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let _ = std::fs::remove_file(path);
            Ok(Some(text))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Removes the draft, tolerating its absence.
pub fn remove_draft(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn composer(text: &str) -> Composer {
        Composer::from_text(text)
    }

    #[test]
    fn insert_and_move_use_char_positions() {
        let mut c = composer("ac");
        c.left();
        c.insert_char('b');
        assert_eq!(c, "abc");
        assert_eq!(c.cursor(), 2);
        assert!(c.left());
        assert!(c.right());
        assert!(c.right(), "moves to the end");
        assert!(!c.right(), "already at the end");
        assert!(c.left());
        assert!(c.left());
        assert!(c.left());
        assert!(!c.left(), "already at the start");
    }

    #[test]
    fn utf8_and_wide_characters_are_never_split() {
        // 中文 and an emoji: 2 and 2 display cells, 3 and 1 `char`s.
        let mut c = composer("你好🙂");
        c.backspace(); // removes the emoji
        assert_eq!(c, "你好");
        c.backspace();
        assert_eq!(c, "你");
        assert_eq!(c.len(), 1, "positions are chars, not bytes");
        c.insert_char('好');
        assert_eq!(c, "你好");
        // The cursor cell counts display width, not chars.
        c.line_start();
        assert_eq!(c.cursor_cell(80), (0, 0));
        c.line_end();
        assert_eq!(c.cursor_cell(80), (0, 4), "two 2-wide characters");
    }

    #[test]
    fn backspace_deletes_before_the_cursor_not_at_the_end() {
        let mut c = composer("abc");
        c.left();
        c.backspace();
        assert_eq!(c, "ac", "the character before the cursor is removed");
        assert_eq!(c.cursor(), 1);
    }

    #[test]
    fn delete_removes_under_the_cursor() {
        let mut c = composer("abc");
        c.left();
        c.left(); // cursor 1
        assert!(c.delete());
        assert_eq!(c, "ac", "the character under the cursor is removed");
        assert_eq!(c.cursor(), 1);
    }

    #[test]
    fn word_movement_and_deletion_are_whitespace_delimited() {
        let mut c = composer("git status --short");
        assert!(c.word_left());
        assert_eq!(c.cursor(), 11, "before `--short`");
        assert!(c.word_left());
        assert_eq!(c.cursor(), 4, "before `status`");
        assert!(c.word_left());
        assert_eq!(c.cursor(), 0);
        assert!(!c.word_left());
        assert!(c.word_right());
        assert_eq!(c.cursor(), 3);

        let mut c = composer("git status");
        assert!(c.delete_word());
        assert_eq!(c, "git ");
        assert!(c.delete_word());
        assert_eq!(c, "");
        assert!(!c.delete_word());
    }

    #[test]
    fn line_start_end_kill_and_multiline_movement_keep_the_column() {
        // Ctrl-A / Ctrl-E are the logical line's start / end, not the buffer's.
        let mut c = composer("hello\nworld");
        assert_eq!(c.cursor(), 11);
        assert!(!c.line_end(), "already at the line end");
        assert!(c.line_start());
        assert_eq!(c.cursor(), 6);
        assert!(!c.kill_to_line_start(), "already at the line start");
        assert!(c.kill_to_line_end());
        assert_eq!(c, "hello\n");
        assert!(!c.kill_to_line_end(), "the line is already empty");

        // Up / down keep the column; a short line clamps it.
        let mut c = composer("hello\nworld");
        c.line_end(); // 11
        assert!(c.up());
        assert_eq!(c.cursor(), 5, "the same column on the previous line");
        assert!(!c.up(), "already on the first line");
        assert!(c.down());
        assert_eq!(c.cursor(), 11);
        assert!(!c.down(), "already on the last line");

        let mut c = composer("hello\nx\nworld");
        c.line_end(); // 13
        assert!(c.up());
        assert_eq!(c.cursor(), 7, "clamped to the end of `x`");
        assert!(c.up());
        assert_eq!(c.cursor(), 1, "column 1 on `hello`");
    }

    #[test]
    fn first_and_last_line_predicates() {
        let mut c = composer("a\nb");
        assert!(
            !c.is_first_line() && c.is_last_line(),
            "the cursor ends on line 2"
        );
        assert!(c.up());
        assert!(c.is_first_line() && !c.is_last_line());
        assert!(c.down());
        assert!(!c.is_first_line() && c.is_last_line());
    }

    #[test]
    fn insert_str_normalizes_carriage_returns_and_never_sends() {
        let mut c = composer("one");
        c.left();
        c.insert_str("a\r\nb\rc");
        assert_eq!(c, "ona\nb\nce");
    }

    #[test]
    fn cursor_cell_follows_soft_wrap() {
        // Width 4: "abcd" then "ef" wraps.
        let mut c = composer("abcdef");
        assert_eq!(c.cursor_cell(4), (1, 2));
        c.line_start();
        assert_eq!(c.cursor_cell(4), (0, 0));
        // A newline forces a row even when the line is short.
        let c = composer("ab\ncd");
        assert_eq!(c.cursor_cell(80), (1, 2));
    }

    #[test]
    fn rows_preserve_spaces_and_wide_characters() {
        assert_eq!(rows("a  b", 40), vec!["a  b".to_string()]);
        assert_eq!(rows("abc", 2), vec!["ab".to_string(), "c".to_string()]);
        assert_eq!(rows("你好", 2), vec!["你".to_string(), "好".to_string()]);
        assert_eq!(rows("a\n", 40), vec!["a".to_string(), String::new()]);
    }

    #[test]
    fn a_draft_round_trips_owner_only_and_an_empty_one_removes_it() {
        let dir = std::env::temp_dir().join(format!("radian-tui-draft-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = draft_path(&dir, "sid");
        assert_eq!(take_draft(&path).unwrap(), None, "no draft yet");

        save_draft(&path, "hello\ndraft").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the draft is owner-only");
        }
        assert_eq!(take_draft(&path).unwrap(), Some("hello\ndraft".to_string()));
        assert!(!path.exists(), "taking the draft removes the file");

        // Saving an empty draft removes any existing one.
        save_draft(&path, "x").unwrap();
        save_draft(&path, "").unwrap();
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_text_puts_the_cursor_at_the_end_and_clear_empties_it() {
        let mut c = composer("");
        c.set_text("hello");
        assert_eq!(c.cursor(), 5);
        c.clear();
        assert!(c.is_empty());
        assert_eq!(c.cursor(), 0);
    }

    #[test]
    fn to_end_moves_the_cursor_to_the_buffer_end_from_the_middle() {
        let mut c = composer("hello");
        c.left();
        c.left(); // cursor 3
        assert_eq!(c.cursor(), 3);
        c.to_end();
        assert_eq!(c.cursor(), 5, "the cursor is at the end");
        assert_eq!(c, "hello", "the text is unchanged");
    }
}
