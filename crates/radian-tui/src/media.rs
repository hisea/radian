//! Images in the transcript (ADR 0028 R10): the data an entry carries, the fixed row run an
//! image occupies, and the decoded protocol state cached across frames.
//!
//! An image part's display facts (name, dimensions, size) live in its `annotations`, so the
//! placeholder line needs no decode. The payload is decoded **once**, when the entry is built:
//! a part whose payload does not decode carries no run at all (its entry reserves no rows, so
//! the placeholder line is the whole of what it shows, ADR 0028 R10), and the decode itself is
//! handed to the [`ImageCache`], which builds the protocol from it on the first frame instead
//! of decoding the payload again.
//!
//! The layout and the drawing both read [`EntryImage::decodable`], so the counting, the
//! drawing and the selection geometry cannot disagree about whether a run exists.
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
    /// Whether the payload decoded when the entry was built (ADR 0028 R10). `false` means the
    /// placeholder line is all there is: the layout reserves no run for it, so no blank hole
    /// is left where the image would have been.
    pub decodable: bool,
}

impl EntryImage {
    /// The image parts of a message, in order. A part without this pipeline's annotations is
    /// left out: it has no placeholder line to stand above the image (ADR 0028 R10).
    ///
    /// The payload is decoded here, once, and the decoded image is kept in `cache` so the
    /// first frame that draws it needs no second decode.
    pub fn from_part(part: &ContentPart, cache: &mut ImageCache) -> Option<Self> {
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
        let mut image = Self {
            data: Arc::from(data.as_str()),
            mime_type: mime_type.clone(),
            width,
            height,
            placeholder,
            // Filled in below, from the one decode this constructor runs.
            decodable: false,
        };
        // Checked once, here: the layout needs the answer before any frame draws, and a
        // broken payload must not reserve rows (ADR 0028 R10). The decode is handed to the
        // cache, so the frame that draws the image builds its protocol from this decode
        // rather than decoding the payload a second time. A payload the cache has already
        // seen (the same image in an earlier entry, or one already known to fail) is not
        // decoded again.
        let key = image.key();
        image.decodable = match cache.known(key) {
            Some(decodable) => decodable,
            None => {
                let decoded = decode(&image);
                let decodable = decoded.is_some();
                cache.remember(key, decoded);
                decodable
            }
        };
        Some(image)
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

/// The images of a message's parts, in order. The decodes go into `cache` (ADR 0028 R10).
pub fn images_of(cache: &mut ImageCache, parts: &[ContentPart]) -> Vec<EntryImage> {
    parts
        .iter()
        .filter_map(|part| EntryImage::from_part(part, cache))
        .collect()
}

/// The byte ranges the placeholder rows occupy in a tool result's **selection text**: its own
/// text, then one `[image: …]` line per image, each introduced by a newline when something
/// precedes it. The layout maps its synthesized placeholder rows to these offsets, and the
/// selection copies the same text, so copy sees the placeholder line and never the run
/// (ADR 0028 R10). Every image has a row, drawn or not: a broken payload still names itself.
pub fn placeholder_spans(text_len: usize, images: &[EntryImage]) -> Vec<(usize, usize)> {
    let mut cursor = text_len;
    let mut spans = Vec::with_capacity(images.len());
    for image in images {
        if cursor > 0 {
            cursor += 1;
        }
        let start = cursor;
        cursor += image.placeholder.len();
        spans.push((start, cursor));
    }
    spans
}

/// Whether a tool result's image rows draw (ADR 0028 R10): when its body is drawn in full —
/// expanded, or a failed result, whose body is never folded (ADR 0028 R4). A user message has
/// no fold, so its images always draw.
pub fn rows_visible(entry: &Entry, modes: DisplayModes) -> bool {
    if entry.kind != EntryKind::ToolResult {
        return true;
    }
    entry.is_error || crate::blocks::expanded(entry, modes.tools.fold == ToolFold::Expanded)
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
/// never on every frame). An entry's decode waits here until the first frame builds the
/// protocol from it; a failure is cached too, so a broken image is not retried each frame and
/// the placeholder line is all the user sees.
#[derive(Default)]
pub struct ImageCache {
    entries: HashMap<u64, Cached>,
    /// Insertion order for the capacity bound (oldest first).
    order: VecDeque<u64>,
}

#[allow(clippy::large_enum_variant)]
enum Cached {
    /// The payload was decoded when the entry was built, waiting for the picker that turns it
    /// into a protocol (ADR 0028 R10: the first frame reuses this decode).
    Decoded(Box<image::DynamicImage>),
    Ready(Box<StatefulProtocol>),
    Failed,
}

impl ImageCache {
    /// Keeps the entry's decode until the first frame asks for the protocol. A payload the
    /// cache already holds (the same image in an earlier entry) keeps the entry it has, so
    /// two entries of one image share one decode and one protocol.
    fn remember(&mut self, key: u64, decoded: Option<image::DynamicImage>) {
        if self.entries.contains_key(&key) {
            return;
        }
        let cached = match decoded {
            Some(decoded) => Cached::Decoded(Box::new(decoded)),
            None => Cached::Failed,
        };
        self.entries.insert(key, cached);
        self.order.push_back(key);
        self.evict();
    }

    /// The decode result already known for this payload: `Some(false)` is a failure.
    fn known(&self, key: u64) -> Option<bool> {
        self.entries
            .get(&key)
            .map(|cached| !matches!(cached, Cached::Failed))
    }

    /// Drops the oldest entries past [`IMAGE_CACHE_CAPACITY`]. A dropped image is decoded
    /// again if it scrolls back into view; the layout keeps its row count either way.
    fn evict(&mut self) {
        while self.entries.len() > IMAGE_CACHE_CAPACITY {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
    }

    /// The protocol for `image`, built from the decode the entry already did. `None` when the
    /// payload does not decode; the caller draws nothing for that run (the placeholder row
    /// stays).
    pub fn protocol(
        &mut self,
        image: &EntryImage,
        picker: &Picker,
    ) -> Option<&mut StatefulProtocol> {
        let key = image.key();
        match self.entries.entry(key) {
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                // The entry's own decode becomes the protocol on the first frame that draws it
                // (ADR 0028 R10: one decode per image). The slot keeps its place in the
                // eviction order.
                if matches!(slot.get(), Cached::Decoded(_)) {
                    let cached = std::mem::replace(slot.get_mut(), Cached::Failed);
                    if let Cached::Decoded(decoded) = cached {
                        slot.insert(Cached::Ready(Box::new(
                            picker.new_resize_protocol(*decoded),
                        )));
                    }
                }
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                // Evicted since the entry was built (or an image built without a cache): the
                // one decode the layout saw is repeated here, and the failure is remembered.
                let cached = if image.decodable {
                    match decode(image) {
                        Some(decoded) => {
                            Cached::Ready(Box::new(picker.new_resize_protocol(decoded)))
                        }
                        None => Cached::Failed,
                    }
                } else {
                    Cached::Failed
                };
                slot.insert(cached);
                self.order.push_back(key);
                self.evict();
            }
        }
        match self.entries.get_mut(&key)? {
            Cached::Ready(protocol) => Some(protocol),
            Cached::Decoded(_) | Cached::Failed => None,
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
        let mut cache = ImageCache::default();
        let entry_image = EntryImage::from_part(&image_part(), &mut cache).expect("an image part");
        assert_eq!(entry_image.placeholder, "[image: shot.gif, 1×1, 1 KB]");
        assert_eq!((entry_image.width, entry_image.height), (1, 1));
        assert!(entry_image.decodable, "the payload decodes");
        // A foreign image part has no placeholder line, so it is not drawn.
        let foreign = ContentPart::Image {
            mime_type: "image/png".to_owned(),
            data: String::new(),
            annotations: None,
            meta: None,
        };
        assert!(EntryImage::from_part(&foreign, &mut cache).is_none());
        // A part with the annotations but a payload that does not decode still names itself:
        // the placeholder stays and no run is reserved (ADR 0028 R10).
        let mut broken = image_part();
        if let ContentPart::Image { data, .. } = &mut broken {
            *data = "not base64 at all".to_owned();
        }
        let broken =
            EntryImage::from_part(&broken, &mut cache).expect("the annotations are intact");
        assert_eq!(broken.placeholder, "[image: shot.gif, 1×1, 1 KB]");
        assert!(!broken.decodable, "the check happens once at ingest");
    }

    /// ADR 0028 R10: one decode per image, however many frames draw it, and a failure is
    /// remembered too (the placeholder row is all that shows).
    #[test]
    fn an_image_decodes_once_and_a_failure_is_cached() {
        let picker = Picker::halfblocks();
        let mut cache = ImageCache::default();
        let before = decodes();
        let image = EntryImage::from_part(&image_part(), &mut cache).expect("an image part");
        assert_eq!(decodes() - before, 1, "building the entry decodes once");
        assert!(cache.protocol(&image, &picker).is_some());
        assert_eq!(
            decodes() - before,
            1,
            "the first frame reuses the entry's decode"
        );
        assert!(cache.protocol(&image, &picker).is_some());
        assert_eq!(
            decodes() - before,
            1,
            "the second call must reuse the decode"
        );
        assert!(cache.is_ready(&image));

        // The hand-built payload claims to be decodable, so the cache tries and remembers the
        // failure rather than retrying it every frame.
        let broken = EntryImage {
            data: Arc::from("not base64 at all"),
            decodable: true,
            ..image
        };
        drop(cache);
        let mut cache = ImageCache::default();
        assert!(cache.protocol(&broken, &picker).is_none());
        let after_failure = decodes();
        assert!(cache.protocol(&broken, &picker).is_none());
        assert_eq!(decodes(), after_failure, "the failure is cached");
        assert!(!cache.is_ready(&broken));

        // A payload already known to fail at ingest is never decoded by the cache.
        let mut part = image_part();
        if let ContentPart::Image { data, .. } = &mut part {
            *data = "not base64 at all".to_owned();
        }
        let mut cache = ImageCache::default();
        let known_bad =
            EntryImage::from_part(&part, &mut cache).expect("the annotations are intact");
        assert!(!known_bad.decodable);
        let before = decodes();
        assert!(cache.protocol(&known_bad, &picker).is_none());
        assert_eq!(decodes(), before, "an undecodable part is not retried");
    }

    /// A part whose payload is the test GIF with `extra` zero bytes appended: distinct data
    /// (so a distinct cache key) that still decodes.
    fn distinct_part(extra: usize) -> ContentPart {
        use base64::Engine;
        let mut bytes = GIF.to_vec();
        bytes.extend(std::iter::repeat_n(0u8, extra));
        ContentPart::Image {
            mime_type: "image/gif".to_owned(),
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
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

    /// ADR 0028 R10: the cache is bounded. Nine distinct images leave the first out of it, so
    /// drawing it again decodes it exactly once, and the frames after that reuse it.
    #[test]
    fn the_ninth_image_evicts_the_first_and_drawing_it_again_decodes_once() {
        let picker = Picker::halfblocks();
        let mut cache = ImageCache::default();
        let images: Vec<EntryImage> = (0..=IMAGE_CACHE_CAPACITY)
            .map(|extra| {
                EntryImage::from_part(&distinct_part(extra), &mut cache).expect("an image")
            })
            .collect();
        assert_eq!(images.len(), IMAGE_CACHE_CAPACITY + 1);
        assert_eq!(
            cache.entries.len(),
            IMAGE_CACHE_CAPACITY,
            "the ninth image evicted the first"
        );

        // Drawing the eight images still in the cache decodes nothing.
        let after_ingest = decodes();
        for image in &images[1..] {
            assert!(cache.protocol(image, &picker).is_some());
        }
        assert_eq!(
            decodes(),
            after_ingest,
            "a cached image is not decoded by a frame"
        );

        // The first image was evicted, so drawing it again costs exactly one decode.
        let before = decodes();
        assert!(cache.protocol(&images[0], &picker).is_some());
        assert_eq!(
            decodes() - before,
            1,
            "an evicted image is decoded once more"
        );
        // And the frames after that reuse it: the image is in the cache again.
        for _ in 0..3 {
            assert!(cache.protocol(&images[0], &picker).is_some());
        }
        assert_eq!(
            decodes() - before,
            1,
            "drawing the re-decoded image again decodes nothing more"
        );
        assert!(cache.is_ready(&images[0]));
    }

    /// ADR 0028 R10: the decoded state of at most [`IMAGE_CACHE_CAPACITY`] images is held,
    /// however many images the transcript pushes and draws.
    #[test]
    fn the_cache_never_holds_more_than_its_capacity() {
        // ADR 0028 R10 and `commands` name 8: the number is not a free implementation detail.
        assert_eq!(IMAGE_CACHE_CAPACITY, 8);
        let picker = Picker::halfblocks();
        let mut cache = ImageCache::default();
        let mut images: Vec<EntryImage> = Vec::new();
        for extra in 0..(IMAGE_CACHE_CAPACITY * 3) {
            images
                .push(EntryImage::from_part(&distinct_part(extra), &mut cache).expect("an image"));
            assert!(
                cache.entries.len() <= IMAGE_CACHE_CAPACITY,
                "adding an image must not grow the cache past its capacity"
            );
            // Draw the newest and a few older ones: an older image re-enters the cache.
            let newest = images.len() - 1;
            let first = newest.saturating_sub(3);
            for older in images[first..=newest].iter() {
                let _ = cache.protocol(older, &picker);
                assert!(
                    cache.entries.len() <= IMAGE_CACHE_CAPACITY,
                    "drawing an image must not grow the cache past its capacity"
                );
            }
        }
        assert_eq!(cache.entries.len(), IMAGE_CACHE_CAPACITY);
    }

    /// ADR 0028 R10: a decode failure is remembered for the image and is never retried, not
    /// even after the failure was pushed out of the cache.
    #[test]
    fn a_failed_decode_is_not_retried_after_eviction() {
        let picker = Picker::halfblocks();
        let mut cache = ImageCache::default();
        let mut broken = image_part();
        if let ContentPart::Image { data, .. } = &mut broken {
            // Valid base64 that is not an image: the decode runs and fails.
            use base64::Engine;
            *data = base64::engine::general_purpose::STANDARD.encode(b"not an image at all");
        }
        let failed = EntryImage::from_part(&broken, &mut cache).expect("the annotations survive");
        assert!(!failed.decodable, "the payload does not decode");

        // Push the failure out of the cache with a full cache's worth of good images.
        for extra in 0..IMAGE_CACHE_CAPACITY {
            EntryImage::from_part(&distinct_part(extra), &mut cache).expect("an image");
        }
        assert!(
            !cache.entries.contains_key(&failed.key()),
            "the failure was evicted"
        );

        let before = decodes();
        assert!(cache.protocol(&failed, &picker).is_none());
        assert_eq!(decodes(), before, "an evicted failure is not decoded again");
        assert!(!cache.is_ready(&failed));
    }

    /// The width keeps the aspect ratio at the fixed row height and never exceeds the body
    /// column (ADR 0028 R10).
    #[test]
    fn the_image_width_follows_the_aspect_ratio_and_the_column() {
        let picker = Picker::halfblocks(); // a 10×20 px cell
        let image = EntryImage::from_part(&image_part(), &mut ImageCache::default())
            .expect("an image part");
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
