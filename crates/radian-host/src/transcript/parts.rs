//! The pure text helpers both folds need: a message's display text, its images, the `!` command
//! block, and the row summary an ACP worker's non-protocol line gets.
//!
//! None of this draws anything; it only decides what an entry's `text` is. They live here so a
//! journal folded by the hub and the same journal folded by the TUI produce character-for-
//! character the same entries.

use std::sync::Arc;

use radian_coding::image;
use radian_core::ContentPart;
use serde_json::{Map, Value};

use super::{Entry, EntryKind, ImagePart};

/// The keys a row's summary is taken from, in this order (ADR 0048 §13).
pub const SUMMARY_KEYS: [&str; 5] = ["text", "message", "command", "content", "summary"];

/// Concatenated text parts, for display.
pub fn text_of(parts: &[ContentPart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// The display text of a user message's parts: an attachment part becomes
/// `[file: P, N lines]`, an image part `[image: <name>, W×H, N KB]` (ADR 0028 R5); neither
/// ever shows its content.
pub fn display_parts(parts: &[ContentPart]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for part in parts {
        match part {
            ContentPart::Text { text, .. } => match part.attachment_path() {
                Some(path) => lines.push(format!("[file: {path}, {} lines]", text.lines().count())),
                None => lines.push(text.clone()),
            },
            ContentPart::Image { .. } => {
                if let Some(line) = image::part_placeholder(part) {
                    lines.push(line);
                }
            }
            _ => {}
        }
    }
    lines.join("\n\n")
}

/// The images of a message's parts, in order, as the fold records them: the wire payload and
/// the display facts, with no decode. A part without this pipeline's annotations is left out:
/// it has no placeholder line to stand above the image (ADR 0028 R10). The renderer decodes
/// the payload; the fold never does.
pub fn image_parts(parts: &[ContentPart]) -> Vec<ImagePart> {
    parts
        .iter()
        .filter_map(|part| {
            let ContentPart::Image {
                mime_type, data, ..
            } = part
            else {
                return None;
            };
            let placeholder = image::part_placeholder(part)?;
            let ContentPart::Image {
                annotations: Some(value),
                ..
            } = part
            else {
                return None;
            };
            let width = u32::try_from(value.get("width")?.as_u64()?).ok()?;
            let height = u32::try_from(value.get("height")?.as_u64()?).ok()?;
            Some(ImagePart {
                data: Arc::from(data.as_str()),
                mime_type: mime_type.clone(),
                width,
                height,
                placeholder,
            })
        })
        .collect()
}

/// The command in a `Ran \`cmd\`` context message, or `None` when the text is not one.
pub fn bash_context_command(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("Ran `")?;
    let end = rest.find('`')?;
    Some(&rest[..end])
}

/// A resumed `!` context message as a Bash block: `$ cmd` (its first line) and the fenced body.
pub fn bash_entry_from_context(text: &str) -> Option<Entry> {
    let command = bash_context_command(text)?;
    let body = text.find('\n').map_or("", |i| &text[i + 1..]);
    Some(Entry::new(
        EntryKind::Bash {
            exclude_from_context: false,
        },
        format!("{command}\n{}", body.trim_end_matches('\n')),
    ))
}

/// The first non-empty summary string: the keys of [`SUMMARY_KEYS`] at the top level first,
/// then the same keys one level down inside every object-valued field.
pub fn first_summary(object: &Map<String, Value>) -> Option<&str> {
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
pub fn line_breaks_to_spaces(text: &str) -> String {
    text.replace("\r\n", " ").replace(['\n', '\r'], " ")
}
