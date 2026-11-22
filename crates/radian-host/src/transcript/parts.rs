//! The pure text helpers the fold needs: a message's display text, its images and the `!`
//! command block.
//!
//! None of this draws anything; it only decides what an entry's `text` is. They live here so a
//! journal folded by `radian attach` and the same journal folded by the TUI produce character-for-
//! character the same entries.

use std::sync::Arc;

use radian_coding::image;
use radian_core::ContentPart;

use super::{Entry, EntryKind, ImagePart};

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
