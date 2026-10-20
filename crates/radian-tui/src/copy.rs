//! Picking the text `/copy` copies: the last **committed** assistant message, or its last
//! fenced code block.
//!
//! Both are pure text functions over the transcript entries. A streaming (`live`) assistant
//! entry is never picked: while a turn streams `/copy` copies the previous committed
//! message, not the in-flight text.

use crate::app::{Entry, EntryKind};

/// The last committed assistant message's markdown source: the text as committed (not the
/// rendered rows, not the reasoning). `None` when no assistant message was committed yet —
/// a streaming entry does not count.
pub fn last_assistant_text(entries: &[Entry]) -> Option<&str> {
    entries
        .iter()
        .rev()
        .find(|entry| entry.kind == EntryKind::Assistant && !entry.live)
        .map(|entry| entry.text.as_str())
}

/// The last fenced (```` ``` ````) code block's content, without the fence lines. The
/// opener may carry an info string and may be longer than three backticks; the closer is a
/// bare run of the same character, at least as long. An unclosed trailing fence counts with
/// what it holds; a line that opens and closes on itself (inline code) is content, never a
/// fence. `None` when there is no block.
pub fn last_fenced_block(text: &str) -> Option<String> {
    let mut last: Option<Vec<&str>> = None;
    let mut current: Option<(char, usize, Vec<&str>)> = None;
    for line in text.lines() {
        match (fence(line), current.take()) {
            // Inside a block: only its own bare closer ends it, everything else is content.
            (marker, Some((open, len, mut block))) => match marker {
                Some((char, close, true)) if char == open && close >= len => {
                    last = Some(block);
                }
                _ => {
                    block.push(line);
                    current = Some((open, len, block));
                }
            },
            // Outside a block: a fence opens one, unless the line closes itself (inline).
            (Some((char, len, _)), None) if !closes_itself(line) => {
                current = Some((char, len, Vec::new()));
            }
            _ => {}
        }
    }
    // A trailing unclosed fence still holds a block (whatever it gathered).
    last.or(current.map(|(_, _, block)| block))
        .map(|block| block.join("\n"))
}

/// A fence run at the line's start: the character, its length, and whether the line holds
/// nothing else (a closer must be bare; an opener may carry an info string).
fn fence(line: &str) -> Option<(char, usize, bool)> {
    let trimmed = line.trim();
    let char = match trimmed.chars().next() {
        Some('`') => '`',
        Some('~') => '~',
        _ => return None,
    };
    let len = trimmed.chars().take_while(|&c| c == char).count();
    if len < 3 {
        return None;
    }
    let bare = trimmed[len..].trim().is_empty();
    Some((char, len, bare))
}

/// Whether the line holds a second fence run after its opener (```` ```code``` ````):
/// inline code, never a block fence.
fn closes_itself(line: &str) -> bool {
    let trimmed = line.trim().trim_start_matches(['`', '~']);
    trimmed.contains("```") || trimmed.contains("~~~")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: EntryKind, text: &str) -> Entry {
        Entry::new(kind, text)
    }

    #[test]
    fn the_last_committed_assistant_message_is_picked() {
        let entries = vec![
            entry(EntryKind::User, "first"),
            entry(EntryKind::Assistant, "old answer"),
            entry(EntryKind::User, "second"),
            entry(EntryKind::Assistant, "new answer"),
        ];
        assert_eq!(last_assistant_text(&entries), Some("new answer"));
    }

    #[test]
    fn a_streaming_entry_never_counts() {
        let mut live = entry(EntryKind::Assistant, "in-flight");
        live.live = true;
        let entries = vec![entry(EntryKind::Assistant, "committed"), live];
        assert_eq!(last_assistant_text(&entries), Some("committed"));

        // Only a live entry: nothing to copy.
        let mut only_live = entry(EntryKind::Assistant, "in-flight");
        only_live.live = true;
        assert_eq!(last_assistant_text(&[only_live]), None);

        assert_eq!(last_assistant_text(&[]), None);
    }

    #[test]
    fn the_last_block_is_picked_without_its_fences() {
        let text =
            "some prose\n```rust\nlet a = 1;\n```\nmore prose\n```python\nprint(2)\n```\ntail";
        assert_eq!(last_fenced_block(text), Some("print(2)".to_owned()));
    }

    #[test]
    fn fence_lines_never_leak_into_the_block() {
        // The info string and longer fences stay on the fence lines, outside the copy.
        let text = "````markdown\n```nested\n````\n";
        assert_eq!(last_fenced_block(text), Some("```nested".to_owned()));
        // One-line inline code is content, never a fence.
        let inline = "use ```code``` here\n```rust\nbody\n```";
        assert_eq!(last_fenced_block(inline), Some("body".to_owned()));
    }

    #[test]
    fn no_block_is_none_and_an_unclosed_fence_counts() {
        assert_eq!(last_fenced_block("just prose, no fences"), None);
        assert_eq!(
            last_fenced_block("prose\n```rust\npartial"),
            Some("partial".to_owned())
        );
    }
}
