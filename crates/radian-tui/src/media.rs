//! Images in the transcript (ADR 0028 R10): the data an entry carries, the fixed row run an
//! image occupies, and the decoded protocol state cached across frames.
//!
//! An image part's display facts (name, dimensions, size) live in its `annotations`, so the
//! placeholder line needs no decode; this module only decodes when a frame actually draws the
//! image, and caches the result per image. The layout counts `IMAGE_HEIGHT_ROWS` empty rows per
//! image whether or not the decode succeeds, so a decode failure never moves the rows below it
//! — it just leaves the run blank (the placeholder line still says what the image was).
//!
//! Nothing here reads a file: the bytes are the part's own base64 payload.

use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use radian_coding::image::part_placeholder;
use radian_core::ContentPart;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;

use crate::app::{Entry, EntryKind};
use crate::tools::{DisplayModes, ToolFold};

/// How many transcript rows one image occupies (ADR 0028 R10). Fixed, so scrolling and the
/// selection geometry do not depend on the image's dimensions or on the terminal's font.
pub const IMAGE_HEIGHT_ROWS: u16 = 12;

/// The most images whose decoded protocol state is kept. Each may hold a decoded copy of up to
/// the 5 MiB image limit; an older image is decoded again if it scrolls back into view.
pub const IMAGE_CACHE_CAPACITY: usize = 8;

/// One image a transcript entry carries: the placeholder line, the wire data and the display
/// facts. `data` is shared (`Arc`) so pushing a message does not copy the base64 text twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryImage {
    /// Standard base64, the part's own payload.
    pub data: Arc<str>,
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    /// `[image: <source>, W×H, N KB]`, the line drawn directly above the image.
    pub placeholder: String,
}

impl EntryImage {
    /// The image parts of a message, in order. A part without this pipeline's annotations is
    /// left out: it has no placeholder line to stand above the image (ADR 0028 R10).
    pub fn from_part(part: &ContentPart) -> Option<Self> {
        let ContentPart::Image {
            mime_type, data, ..
        } = part
        else {
            return None;
        };
        let placeholder = part_placeholder(part)?;
        let annotations = match part {
            ContentPart::Image {
                annotations: Some(value),
                ..
            } => value,
            _ => return None,
        };
        let width = u32::try_from(annotations.get("width")?.as_u64()?).ok()?;
        let height = u32::try_from(annotations.get("height")?.as_u64()?).ok()?;
        Some(Self {
            data: Arc::from(data.as_str()),
            mime_type: mime_type.clone(),
            width,
            height,
            placeholder,
        })
    }

    /// A stable key for the decode cache: the payload plus its display dimensions. Two entries
    /// that carry the same image share one decoded protocol.
    fn key(&self) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.mime_type.hash(&mut hasher);
        self.width.hash(&mut hasher);
        self.height.hash(&mut hasher);
        self.data.hash(&mut hasher);
        hasher.finish()
    }
}

/// The images of a message's parts, in order.
pub fn images_of(parts: &[ContentPart]) -> Vec<EntryImage> {
    parts.iter().filter_map(EntryImage::from_part).collect()
}

/// Whether a tool result's image rows draw (ADR 0028 R10): only when its body is drawn in
/// full. A user message has no fold, so its images always draw.
pub fn rows_visible(entry: &Entry, modes: DisplayModes) -> bool {
    if entry.kind != EntryKind::ToolResult {
        return true;
    }
    crate::blocks::expanded(entry, modes.tools.fold == ToolFold::Expanded)
        || (modes.tools.fold == ToolFold::Errors && entry.is_error)
}

/// The image's width in columns: its aspect ratio at [`IMAGE_HEIGHT_ROWS`] rows, from the
/// terminal's cell pixel size, clamped to the body column (ADR 0028 R10).
pub fn image_columns(picker: &Picker, image: &EntryImage, max_columns: u16) -> u16 {
    let font = picker.font_size();
    let cell_width = u32::from(font.width.max(1));
    let cell_height = u32::from(font.height.max(1));
    if image.height == 0 || max_columns == 0 {
        return 0;
    }
    // The run's pixel height, then the width that keeps the aspect ratio.
    let pixel_height = u32::from(IMAGE_HEIGHT_ROWS) * cell_height;
    let pixel_width = (u64::from(pixel_height) * u64::from(image.width))
        .div_ceil(u64::from(image.height))
        .min(u64::from(u32::MAX)) as u32;
    let columns = pixel_width.div_ceil(cell_width).max(1);
    u16::try_from(columns)
        .unwrap_or(u16::MAX)
        .min(max_columns)
        .max(1)
}

/// Decoded image protocols, keyed by [`EntryImage::key`] (ADR 0028 R10: decode once per image,
/// never on every frame). A failed decode is cached too, so a broken image is not retried each
/// frame; the placeholder line is all the user sees then.
#[derive(Default)]
pub struct ImageCache {
    entries: HashMap<u64, Cached>,
    /// Insertion order for the capacity bound (oldest first).
    order: VecDeque<u64>,
}

#[allow(clippy::large_enum_variant)]
enum Cached {
    Ready(Box<StatefulProtocol>),
    Failed,
}

impl ImageCache {
    /// The protocol for `image`, decoding it on first use. `None` when the payload does not
    /// decode; the caller draws nothing for that run (the placeholder row stays).
    pub fn protocol(
        &mut self,
        image: &EntryImage,
        picker: &Picker,
    ) -> Option<&mut StatefulProtocol> {
        let key = image.key();
        if let std::collections::hash_map::Entry::Vacant(slot) = self.entries.entry(key) {
            let cached = match decode(image) {
                Some(decoded) => Cached::Ready(Box::new(picker.new_resize_protocol(decoded))),
                None => Cached::Failed,
            };
            slot.insert(cached);
            self.order.push_back(key);
            while self.entries.len() > IMAGE_CACHE_CAPACITY {
                if let Some(oldest) = self.order.pop_front() {
                    self.entries.remove(&oldest);
                }
            }
        }
        match self.entries.get_mut(&key)? {
            Cached::Ready(protocol) => Some(protocol),
            Cached::Failed => None,
        }
    }

    /// Whether the image was decoded and is ready to draw, without creating the protocol (a
    /// frame asks this before touching the picker).
    pub fn is_ready(&self, image: &EntryImage) -> bool {
        matches!(self.entries.get(&image.key()), Some(Cached::Ready(_)))
    }
}

/// Decodes one image part's base64 payload into an image the picker can encode.
fn decode(image: &EntryImage) -> Option<image::DynamicImage> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&*image.data)
        .ok()?;
    note_decode();
    image::load_from_memory(&bytes).ok()
}

// Test-only: how many images have been decoded on this thread. Thread-local so parallel tests
// do not share a process-global counter.
#[cfg(test)]
thread_local! {
    static DECODES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn note_decode() {
    DECODES.with(|count| count.set(count.get() + 1));
}

#[cfg(not(test))]
fn note_decode() {}

/// The number of images decoded so far on this thread (test-only).
#[cfg(test)]
pub fn decodes() -> u64 {
    DECODES.with(std::cell::Cell::get)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A 1×1 GIF, the smallest decodable image this pipeline accepts (43 bytes).
    const GIF: &[u8] = &[
        0x47, 0x49, 0x46, 0x38, 0x39, 0x61, // GIF89a
        0x01, 0x00, 0x01, 0x00, // 1×1
        0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, // global colour table
        0x21, 0xf9, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, // graphic control
        0x2c, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, // image descriptor
        0x02, 0x02, 0x44, 0x01, 0x00, 0x3b, // image data + trailer
    ];

    fn image_part() -> ContentPart {
        use base64::Engine;
        ContentPart::Image {
            mime_type: "image/gif".to_owned(),
            data: base64::engine::general_purpose::STANDARD.encode(GIF),
            annotations: Some(json!({
                "kind": radian_coding::image::IMAGE_KIND,
                "name": "shot.gif",
                "width": 1,
                "height": 1,
                "bytes": GIF.len(),
            })),
            meta: None,
        }
    }

    #[test]
    fn an_image_part_becomes_the_placeholder_and_the_data() {
        let entry_image = EntryImage::from_part(&image_part()).expect("an image part");
        assert_eq!(entry_image.placeholder, "[image: shot.gif, 1×1, 1 KB]");
        assert_eq!((entry_image.width, entry_image.height), (1, 1));
        // A foreign image part has no placeholder line, so it is not drawn.
        let foreign = ContentPart::Image {
            mime_type: "image/png".to_owned(),
            data: String::new(),
            annotations: None,
            meta: None,
        };
        assert!(EntryImage::from_part(&foreign).is_none());
    }

    /// ADR 0028 R10: one decode per image, however many frames draw it, and a failure is
    /// cached too (the placeholder row is all that shows).
    #[test]
    fn an_image_decodes_once_and_a_failure_is_cached() {
        let picker = Picker::halfblocks();
        let image = EntryImage::from_part(&image_part()).expect("an image part");
        let mut cache = ImageCache::default();
        let before = decodes();
        assert!(cache.protocol(&image, &picker).is_some());
        assert!(cache.protocol(&image, &picker).is_some());
        assert_eq!(
            decodes() - before,
            1,
            "the second call must reuse the decode"
        );
        assert!(cache.is_ready(&image));

        let broken = EntryImage {
            data: Arc::from("not base64 at all"),
            ..image
        };
        drop(cache);
        let mut cache = ImageCache::default();
        assert!(cache.protocol(&broken, &picker).is_none());
        assert!(cache.protocol(&broken, &picker).is_none());
        assert!(!cache.is_ready(&broken));
    }

    /// The width keeps the aspect ratio at the fixed row height and never exceeds the body
    /// column (ADR 0028 R10).
    #[test]
    fn the_image_width_follows_the_aspect_ratio_and_the_column() {
        let picker = Picker::halfblocks(); // a 10×20 px cell
        let image = EntryImage::from_part(&image_part()).expect("an image part");
        // A square image: 12 rows × 20 px = 240 px tall, so 240 px / 10 px = 24 columns.
        assert_eq!(image_columns(&picker, &image, 80), 24);
        // A column narrower than that wins.
        assert_eq!(image_columns(&picker, &image, 10), 10);
        // A 2:1 image is twice as wide at the same height.
        let wide = EntryImage {
            width: 2,
            ..image.clone()
        };
        assert_eq!(image_columns(&picker, &wide, 80), 48);
        // And a 1:2 image is half as wide.
        let tall = EntryImage { height: 2, ..image };
        assert_eq!(image_columns(&picker, &tall, 80), 12);
    }
}
