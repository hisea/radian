//! Fitting text to a display width: the one wrap the transcript, the selection mapping and the
//! find geometry all share ([`wrap_char_offsets`]), the renderer's hot-path string builder
//! ([`wrap`]), the non-allocating line count ([`wrap_line_count`]), and the clip that cuts a
//! string to a column budget without splitting a wide character ([`clip_display`]).

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// One wrapped line as [`wrap_char_offsets`] sees it: the chars on the line (as byte offsets
/// into the wrapped text, each on a char boundary) and the **gap** just after the line.
///
/// `end` is where a click past the line's last cell lands: the offset just after the last
/// char, or, for an empty line, the offset of its terminating `\n` (or the text length for
/// the final paragraph). Because `wrap` collapses space runs, `end` is not necessarily where
/// the next line's first char sits; that in-between whitespace is exactly what a selection
/// across the break copies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrappedLine {
    pub chars: Vec<usize>,
    pub end: usize,
}

/// The same wrap [`wrap`] performs, but reporting *where each rendered char came from*: per
/// line, the byte offsets (into `text`) of the chars on it, in order, plus the line's end gap
/// (see [`WrappedLine`]). One algorithm, two views: `wrap` is derived from this, so a cell →
/// offset mapping through [`crate::select`] can never drift from what is rendered.
pub fn wrap_char_offsets(text: &str, width: usize) -> Vec<WrappedLine> {
    let width = width.max(1);
    let mut out: Vec<WrappedLine> = Vec::new();
    // The byte offsets of the chars on the line being built, the line's display width, and
    // the end gap so far (just after the last pushed char). The gap starts at 0 and is
    // corrected to the `\n`'s offset for an empty line, and to `text.len()` for an empty
    // final paragraph, when the line is finished.
    let mut chars: Vec<usize> = Vec::new();
    let mut line_width = 0usize;
    let mut end = 0usize;
    // One buffered space (its byte offset), flushed only when a following char stays on the
    // line — so space runs collapse and trail away (the `wrap` semantics, unchanged).
    let mut pending_space: Option<usize> = None;
    for (idx, ch) in text.char_indices() {
        if ch == '\n' {
            let empty = chars.is_empty();
            out.push(WrappedLine {
                chars: std::mem::take(&mut chars),
                end: if empty { idx } else { end },
            });
            line_width = 0;
            end = idx + 1;
            pending_space = None;
            continue;
        }
        if ch == ' ' {
            // A space only joins a line that already has a char on it; leading spaces and
            // repeats collapse away.
            if !chars.is_empty() {
                pending_space = Some(idx);
            }
            continue;
        }
        // A tab occupies four columns (ADR 0028 R9's rule, one rule for every text the
        // transcript wraps); it is not a space, so it never collapses.
        let ch_width = if ch == '\t' {
            4
        } else {
            ch.width().unwrap_or(0)
        };
        let extra = usize::from(pending_space.is_some());
        if line_width + extra + ch_width > width && line_width > 0 {
            out.push(WrappedLine {
                chars: std::mem::take(&mut chars),
                end,
            });
            line_width = 0;
            pending_space = None;
        }
        if let Some(space) = pending_space.take() {
            chars.push(space);
            line_width += 1;
        }
        chars.push(idx);
        line_width += ch_width;
        end = idx + ch.len_utf8();
    }
    out.push(WrappedLine {
        chars,
        end: if end == 0 { text.len() } else { end },
    });
    out
}

/// Hard-wraps `text` to `width` display columns, breaking on spaces where possible and on
/// character boundaries otherwise (CJK has no spaces). Newlines start a new line.
///
/// This is the renderer's hot path (every visible entry, every frame), so it stays the
/// plain string builder. [`wrap_char_offsets`] is the same algorithm reporting *where each
/// rendered char came from*; [`crate::select`] maps cells through it, and the two are held
/// together by the `wrap_and_wrap_char_offsets_agree` test — change either, change both.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        let mut line_width = 0usize;
        let mut pending_space = false;
        for ch in paragraph.chars() {
            if ch == ' ' {
                pending_space = !line.is_empty();
                continue;
            }
            let ch_width = if ch == '\t' {
                4
            } else {
                ch.width().unwrap_or(0)
            };
            let extra = usize::from(pending_space);
            if line_width + extra + ch_width > width && line_width > 0 {
                out.push(std::mem::take(&mut line));
                line_width = 0;
                pending_space = false;
            }
            if pending_space {
                line.push(' ');
                line_width += 1;
                pending_space = false;
            }
            line.push(ch);
            line_width += ch_width;
        }
        out.push(line);
    }
    out
}

/// How many lines [`wrap`] would produce, without building any of them. The virtualization
/// budget (ADR 0028 R7) measures every entry on the first frame and after a resize; this is
/// that path, so it must not allocate.
pub fn wrap_line_count(text: &str, width: usize) -> usize {
    let width = width.max(1);
    let mut lines = 0usize;
    for paragraph in text.split('\n') {
        let mut line_width = 0usize;
        let mut pending_space = false;
        let mut any = false;
        for ch in paragraph.chars() {
            if ch == ' ' {
                // A space only joins a line that already has content (`wrap`'s
                // `!line.is_empty()`): leading spaces are dropped.
                pending_space = any;
                continue;
            }
            let ch_width = if ch == '\t' {
                4
            } else if ch.is_ascii() {
                // `UnicodeWidthChar::width` is a table lookup; ASCII is by far the common case
                // (the first frame counts ~40 MiB of tool output). Printable ASCII is 1, the
                // other ASCII (control) is 0, exactly as `width` reports.
                usize::from((0x20..0x7f).contains(&(ch as u8)))
            } else {
                ch.width().unwrap_or(0)
            };
            let extra = usize::from(pending_space);
            if line_width + extra + ch_width > width && line_width > 0 {
                lines += 1;
                line_width = 0;
                pending_space = false;
            }
            if pending_space {
                line_width += 1;
                pending_space = false;
            }
            line_width += ch_width;
            any = true;
        }
        lines += 1;
    }
    lines
}

/// Cuts `text` to `width` display columns, never splitting a wide character (ADR 0024 Q7: a
/// narrow terminal clips instead of wrapping).
/// Cuts `text` to `width` display cells (the question form's tab labels use it).
pub(crate) fn clip_display(text: &str, width: usize) -> String {
    clip_columns(text, width)
}

pub(super) fn clip_columns(text: &str, width: usize) -> String {
    if width == 0 || UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let cell = ch.width().unwrap_or(0);
        if used + cell + 1 > width {
            break;
        }
        out.push(ch);
        used += cell;
    }
    out.push('…');
    out
}
