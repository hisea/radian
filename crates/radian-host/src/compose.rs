//! The one user-message composer (ADR 0018, ADR 0005): text, attached files and images into
//! the message's [`ContentPart`]s.
//!
//! The TUI's send path (a dropped image, an `@` file) and the process control port both end
//! in [`user_message`], so a message typed at the keyboard and one sent from the hub carry the
//! same parts. The caps are the composer's own: one file at most [`MAX_FILE_BYTES`], all files
//! together at most [`MAX_TOTAL_BYTES`].

use radian_coding::image::LoadedImage;
use radian_core::{ContentPart, Message, MessageMeta};

/// The largest single file that may be attached (100 KiB).
pub const MAX_FILE_BYTES: usize = 100 * 1024;

/// The largest total of attached file contents (400 KiB).
pub const MAX_TOTAL_BYTES: usize = 400 * 1024;

/// One attached file: its path (workspace-relative in the TUI, the client's name over the
/// control port) and its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub path: String,
    pub content: String,
}

/// The message one send carries (ADR 0018): the user's text as one part, one
/// [`ContentPart::Image`] per image, then one [`ContentPart::attachment`] part per file. The
/// provider sees the same text it always did (joined), but the parts are separate, so Core can
/// replace an attachment with a placeholder when it keeps the user's words verbatim.
pub fn user_message(text: &str, attachments: &[Attachment], images: &[LoadedImage]) -> Message {
    // An image-only message (a dropped path is removed from the text) has no text part: an
    // empty `Text` part would be sent to the adapters and draw a blank transcript line.
    let mut content = Vec::new();
    if !text.trim().is_empty() {
        content.push(ContentPart::text(text));
    }
    content.extend(images.iter().map(LoadedImage::part));
    for attachment in attachments {
        content.push(ContentPart::attachment(
            attachment.content.clone(),
            attachment.path.clone(),
        ));
    }
    Message::User {
        content,
        meta: MessageMeta::default(),
    }
}
