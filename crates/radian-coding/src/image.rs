//! The one image pipeline: file bytes or host bytes → checks → [`ContentPart::Image`].
//!
//! Every entry point (`read`, TUI `@path`, a dropped path, `radian exec --image`) ends in
//! [`load`]; a path input uses [`load_path`], the shared bounded read. The kind comes from the
//! **magic number**, never from the extension: PNG, JPEG,
//! GIF and WebP are images for radian; BMP and everything else are not. The limits are
//! constants, not configuration: raw bytes ≤ [`MAX_IMAGE_BYTES`], each side ≤
//! [`MAX_IMAGE_SIDE`]. Dimensions come from the header with `imagesize` (no decode, no
//! re-encode, no local resize).
//!
//! The `ContentPart::Image` this builds carries its display facts in `annotations`
//! (`kind = "image"`, `name`, `width`, `height`, `bytes`), so the transcript's
//! `[image: <name>, W×H, N KB]` line and journal replay need no decode. The compaction
//! layer (ADR 0018) keeps reading the header from `data` as before.

use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use radian_core::ContentPart;
use serde_json::json;

/// The largest raw image accepted (5 MiB, the Anthropic per-image limit).
pub const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

/// The largest accepted image side in pixels (the Anthropic limit).
pub const MAX_IMAGE_SIDE: u32 = 8_000;

/// Above this long side the provider scales the image down; `read` says so in its text.
pub const SCALE_NOTICE_SIDE: u32 = 1_568;

/// The `annotations.kind` this pipeline sets on its image parts.
pub const IMAGE_KIND: &str = "image";

/// Whether the current model takes images. The host supplies the answer (it reads the catalog
/// for the **current** route); a tool set without one counts as taking images, the same rule
/// the catalog applies to an unknown model.
pub type ImageSupport = Arc<dyn Fn() -> bool + Send + Sync>;

/// The default capability: images are taken.
pub fn images_allowed() -> ImageSupport {
    Arc::new(|| true)
}

/// An image kind radian accepts, identified by its magic number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl ImageKind {
    /// The kind from the leading bytes, or `None` when they are not one of the four formats.
    pub fn from_magic(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some(Self::Png)
        } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            Some(Self::Jpeg)
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Some(Self::Gif)
        } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
            Some(Self::Webp)
        } else {
            None
        }
    }

    pub fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
        }
    }
}

/// Why bytes did not become an image. The display text names the limit that was crossed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageError {
    /// The magic number is not PNG, JPEG, GIF or WebP.
    NotImage,
    /// The raw bytes are over [`MAX_IMAGE_BYTES`].
    TooLarge { bytes: u64 },
    /// A side is over [`MAX_IMAGE_SIDE`].
    TooManyPixels { width: u32, height: u32 },
    /// The magic number matched but the header could not be read.
    Unreadable,
}

impl std::fmt::Display for ImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotImage => write!(f, "not a PNG, JPEG, GIF or WebP image"),
            Self::TooLarge { bytes } => write!(
                f,
                "{bytes} bytes, over the {} MiB image limit",
                MAX_IMAGE_BYTES / (1024 * 1024)
            ),
            Self::TooManyPixels { width, height } => write!(
                f,
                "{width}×{height} px, over the {MAX_IMAGE_SIDE} px per-side image limit"
            ),
            Self::Unreadable => write!(f, "the image header could not be read"),
        }
    }
}

impl std::error::Error for ImageError {}

/// An image that passed every check, ready to become a [`ContentPart::Image`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedImage {
    pub name: String,
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
    /// The raw (decoded) byte count, not the base64 length.
    pub bytes: usize,
    /// Standard base64 with padding, the provider wire form.
    pub data: String,
}

impl LoadedImage {
    /// The image part, with the display facts in `annotations`.
    pub fn part(&self) -> ContentPart {
        ContentPart::Image {
            mime_type: self.mime.to_string(),
            data: self.data.clone(),
            annotations: Some(json!({
                "kind": IMAGE_KIND,
                "name": self.name,
                "width": self.width,
                "height": self.height,
                "bytes": self.bytes,
            })),
            meta: None,
        }
    }

    /// `[image: <name>, W×H, N KB]`, the transcript placeholder line.
    pub fn placeholder(&self) -> String {
        placeholder(&self.name, self.width, self.height, self.bytes)
    }
}

/// A user message carrying `text` and each image, in order (the `exec --image` shape; the TUI
/// adds its `@` attachments around the same parts).
pub fn message(text: impl Into<String>, images: &[LoadedImage]) -> radian_core::Message {
    let text = text.into();
    // An image-only message (`--image` with an empty prompt) has no text part: an empty `Text`
    // part would reach the adapters and draw a blank transcript line.
    let mut content = Vec::new();
    if !text.trim().is_empty() {
        content.push(ContentPart::text(text));
    }
    content.extend(images.iter().map(LoadedImage::part));
    radian_core::Message::User {
        content,
        meta: radian_core::MessageMeta::default(),
    }
}

/// The transcript placeholder line for one image.
pub fn placeholder(name: &str, width: u32, height: u32, bytes: usize) -> String {
    format!(
        "[image: {name}, {width}×{height}, {} KB]",
        bytes.div_ceil(1024)
    )
}

/// The placeholder line of an image part built by this pipeline. `None` for a part without
/// the display annotations (a foreign image part); the caller then leaves it out, as export
/// already does.
pub fn part_placeholder(part: &ContentPart) -> Option<String> {
    let ContentPart::Image {
        annotations: Some(value),
        ..
    } = part
    else {
        return None;
    };
    if value.get("kind").and_then(serde_json::Value::as_str) != Some(IMAGE_KIND) {
        return None;
    }
    let name = value.get("name").and_then(serde_json::Value::as_str)?;
    let width = u32::try_from(value.get("width").and_then(serde_json::Value::as_u64)?).ok()?;
    let height = u32::try_from(value.get("height").and_then(serde_json::Value::as_u64)?).ok()?;
    let bytes = usize::try_from(value.get("bytes").and_then(serde_json::Value::as_u64)?).ok()?;
    Some(placeholder(name, width, height, bytes))
}

/// The dimensions from a header prefix, when the prefix holds the whole header. `None` when
/// it does not (the caller may read more) or the parser cannot read it.
pub fn header_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let size = imagesize::blob_size(bytes).ok()?;
    let width = u32::try_from(size.width).ok()?;
    let height = u32::try_from(size.height).ok()?;
    (width > 0 && height > 0).then_some((width, height))
}

/// The one pipeline: magic number → limits → part facts. `name` is the display source (a file
/// name, `clipboard`); `bytes` are the raw file or clipboard bytes.
pub fn load(name: &str, bytes: &[u8]) -> Result<LoadedImage, ImageError> {
    let kind = ImageKind::from_magic(bytes).ok_or(ImageError::NotImage)?;
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return Err(ImageError::TooLarge {
            bytes: bytes.len() as u64,
        });
    }
    let (width, height) = header_dimensions(bytes).ok_or(ImageError::Unreadable)?;
    if width > MAX_IMAGE_SIDE || height > MAX_IMAGE_SIDE {
        return Err(ImageError::TooManyPixels { width, height });
    }
    Ok(LoadedImage {
        name: name.to_owned(),
        mime: kind.mime(),
        width,
        height,
        bytes: bytes.len(),
        data: base64_encode(bytes),
    })
}

/// The bounded head read for a path input: enough for every magic number and for `imagesize`
/// to read an ordinary header. The rest is read only after the metadata size check passes.
pub const MAX_HEAD_BYTES: usize = 64 * 1024;

/// What [`load_path`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathImage {
    /// An accepted image, read in full.
    Loaded(LoadedImage),
    /// The file exists but its magic number is not PNG, JPEG, GIF or WebP.
    NotImage,
    /// The path cannot be opened or read, or is not a regular file.
    Unreadable(String),
    /// It is an image, but over a limit or with an unreadable header.
    Unusable(String),
}

/// The shared bounded path read used by the TUI's dropped paths and `exec --image`: metadata
/// size and a [`MAX_HEAD_BYTES`] head first, the body only for an accepted image. A non-image
/// never allocates its body, and an over-limit image is refused before its body is read.
pub fn load_path(path: &Path) -> PathImage {
    let Ok(mut file) = std::fs::File::open(path) else {
        return PathImage::Unreadable("cannot open the file".into());
    };
    let Ok(meta) = file.metadata() else {
        return PathImage::Unreadable("cannot read the file's metadata".into());
    };
    if !meta.is_file() {
        return PathImage::Unreadable("not a regular file".into());
    }
    let total = meta.len();
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let mut head = Vec::with_capacity(MAX_HEAD_BYTES.min(total as usize));
    let mut buf = [0u8; 8192];
    while head.len() < MAX_HEAD_BYTES {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                note_read(n);
                head.extend_from_slice(&buf[..n]);
            }
            Err(error) => return PathImage::Unreadable(error.to_string()),
        }
    }
    if ImageKind::from_magic(&head).is_none() {
        return PathImage::NotImage;
    }
    // A candidate image: the byte limit comes from metadata, before any body read.
    if total > MAX_IMAGE_BYTES {
        return PathImage::Unusable(ImageError::TooLarge { bytes: total }.to_string());
    }
    // When the head already holds the dimensions, a per-side over-limit is refused here too.
    if let Some((width, height)) = header_dimensions(&head)
        && (width > MAX_IMAGE_SIDE || height > MAX_IMAGE_SIDE)
    {
        return PathImage::Unusable(ImageError::TooManyPixels { width, height }.to_string());
    }
    // The accepted image (at most 5 MiB by the check above): read the rest. The cap also holds
    // if the file grew after the metadata read.
    let want = total.min(MAX_IMAGE_BYTES) as usize;
    let mut bytes = head;
    bytes.reserve(want.saturating_sub(bytes.len()));
    while bytes.len() < want {
        let room = (want - bytes.len()).min(buf.len());
        match file.read(&mut buf[..room]) {
            Ok(0) => break,
            Ok(n) => {
                note_read(n);
                bytes.extend_from_slice(&buf[..n]);
            }
            Err(error) => return PathImage::Unreadable(error.to_string()),
        }
    }
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return PathImage::Unusable(
            ImageError::TooLarge {
                bytes: bytes.len() as u64,
            }
            .to_string(),
        );
    }
    match load(&name, &bytes) {
        Ok(image) => PathImage::Loaded(image),
        Err(ImageError::NotImage) => PathImage::NotImage,
        Err(error) => PathImage::Unusable(error.to_string()),
    }
}

// Test-only count of the bytes `load_path` reads, so a test can prove a huge non-image is not
// read whole. Compiled out of a normal build.
#[cfg(any(test, feature = "testing"))]
std::thread_local! {
    static PATH_BYTES_READ: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(any(test, feature = "testing"))]
pub fn reset_path_read_bytes() {
    PATH_BYTES_READ.with(|bytes| bytes.set(0));
}

#[cfg(any(test, feature = "testing"))]
pub fn path_read_bytes() -> usize {
    PATH_BYTES_READ.with(std::cell::Cell::get)
}

fn note_read(n: usize) {
    #[cfg(any(test, feature = "testing"))]
    PATH_BYTES_READ.with(|bytes| bytes.set(bytes.get() + n));
    #[cfg(not(any(test, feature = "testing")))]
    let _ = n;
}

/// The `read` tool's text part for an image (ADR 0018: the non-vision line is added only when
/// the current model is known not to take images).
pub fn read_description(image: &LoadedImage, supports_images: bool) -> String {
    let mut text = format!(
        "Read image file [{}] {}×{}",
        image.mime, image.width, image.height
    );
    if image.width.max(image.height) > SCALE_NOTICE_SIDE {
        text.push_str(&format!(
            "\nThe long side is over {SCALE_NOTICE_SIDE} px; the provider scales it down."
        ));
    }
    if !supports_images {
        text.push_str(
            "\n[Current model does not support images. The image will be omitted from this request.]",
        );
    }
    text
}

/// Standard base64 (RFC 4648 §4) with `=` padding. The wire form every adapter expects; the
/// crate takes no encoder dependency for these few lines.
pub fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal PNG header with these dimensions (24 bytes: signature, IHDR, width, height).
    /// `imagesize` reads only the header, so the rest may be anything.
    pub(crate) fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.push(8); // bit depth
        bytes.push(6); // colour type (RGBA)
        bytes.extend_from_slice(&[0, 0, 0]); // compression, filter, interlace
        bytes.extend_from_slice(&[0; 4]); // a CRC the header parser does not check
        bytes
    }

    #[test]
    fn the_kind_comes_from_the_magic_number() {
        assert_eq!(ImageKind::from_magic(&png(4, 5)), Some(ImageKind::Png));
        assert_eq!(
            ImageKind::from_magic(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some(ImageKind::Jpeg)
        );
        assert_eq!(ImageKind::from_magic(b"GIF89a...."), Some(ImageKind::Gif));
        assert_eq!(ImageKind::from_magic(b"GIF87a...."), Some(ImageKind::Gif));
        assert_eq!(
            ImageKind::from_magic(b"RIFF\x24\x00\x00\x00WEBPVP8 "),
            Some(ImageKind::Webp)
        );
        // BMP is deliberately not an image for radian.
        assert_eq!(ImageKind::from_magic(b"BM\x00\x00"), None);
        assert_eq!(ImageKind::from_magic(b"plain text"), None);
    }

    #[test]
    fn the_pipeline_reads_dimensions_and_builds_a_part() {
        let bytes = png(1200, 800);
        let image = load("shot.png", &bytes).expect("a valid PNG header");
        assert_eq!(image.mime, "image/png");
        assert_eq!(image.width, 1200);
        assert_eq!(image.height, 800);
        assert_eq!(image.bytes, bytes.len());
        assert_eq!(image.data, base64_encode(&bytes));
        assert!(image.data.ends_with('=') || image.data.ends_with('A'));
        let part = image.part();
        assert_eq!(part_placeholder(&part), Some(image.placeholder()));
        assert_eq!(image.placeholder(), "[image: shot.png, 1200×800, 1 KB]");
    }

    /// A unique scratch directory removed on drop.
    fn scratch(label: &str) -> crate::test_support::TempDir {
        crate::test_support::TempDir::with_label("radian-image", label)
    }

    #[test]
    fn a_valid_path_loads_the_image() {
        let dir = scratch("valid");
        let path = dir.path().join("shot.png");
        std::fs::write(&path, png(10, 10)).unwrap();
        let PathImage::Loaded(image) = load_path(&path) else {
            panic!("a valid PNG loads");
        };
        assert_eq!(image.name, "shot.png");
        assert_eq!(image.width, 10);
    }

    #[test]
    fn a_text_file_is_not_an_image() {
        let dir = scratch("text");
        let path = dir.path().join("notes.png");
        std::fs::write(&path, "plain text").unwrap();
        assert_eq!(load_path(&path), PathImage::NotImage);
    }

    #[test]
    fn a_missing_path_is_unreadable() {
        let dir = scratch("missing");
        assert!(matches!(
            load_path(&dir.path().join("nope.png")),
            PathImage::Unreadable(_)
        ));
    }

    /// A huge sparse non-image must not be read whole: the head is enough to say `NotImage`.
    #[test]
    fn a_large_non_image_is_refused_from_its_head() {
        let dir = scratch("huge-non-image");
        let path = dir.path().join("huge.bin");
        let mut file = std::fs::File::create(&path).unwrap();
        std::io::Write::write_all(&mut file, b"not an image").unwrap();
        file.set_len(2 * 1024 * 1024 * 1024).unwrap();
        drop(file);
        reset_path_read_bytes();
        assert_eq!(load_path(&path), PathImage::NotImage);
        let read = path_read_bytes();
        assert!(
            read > 0 && read <= MAX_HEAD_BYTES,
            "read {read} bytes of a 2 GiB non-image"
        );
    }

    /// An over-limit image is refused from metadata, never by reading its body.
    #[test]
    fn an_over_limit_image_is_refused_before_its_body_is_read() {
        let dir = scratch("huge-image");
        let path = dir.path().join("huge.png");
        let mut file = std::fs::File::create(&path).unwrap();
        std::io::Write::write_all(&mut file, &png(10, 10)).unwrap();
        file.set_len(MAX_IMAGE_BYTES + 1).unwrap();
        drop(file);
        reset_path_read_bytes();
        let outcome = load_path(&path);
        assert!(
            matches!(&outcome, PathImage::Unusable(reason) if reason.contains("5 MiB")),
            "{outcome:?}"
        );
        let read = path_read_bytes();
        assert!(read > 0, "the head must be read");
        assert!(read <= MAX_HEAD_BYTES, "read {read} bytes");
        assert!((read as u64) < MAX_IMAGE_BYTES, "the body was read");
    }

    /// A header whose dimensions are already over 8000 px is refused from the head too.
    #[test]
    fn an_over_limit_side_is_refused_from_its_header() {
        let dir = scratch("wide-image");
        let path = dir.path().join("wide.png");
        std::fs::write(&path, png(9000, 10)).unwrap();
        reset_path_read_bytes();
        let outcome = load_path(&path);
        assert!(
            matches!(&outcome, PathImage::Unusable(reason) if reason.contains("8000")),
            "{outcome:?}"
        );
        assert!(path_read_bytes() > 0, "the head must be read");
        assert!(path_read_bytes() <= MAX_HEAD_BYTES);
    }

    #[test]
    fn the_limits_are_named_in_the_error() {
        let e = load("x", b"not an image").unwrap_err();
        assert_eq!(e, ImageError::NotImage);
        // Over the byte limit is decided before the header is parsed; a real header keeps the
        // error about bytes, not about dimensions.
        let mut huge = png(10, 10);
        huge.resize(5 * 1024 * 1024 + 1, 0);
        assert!(matches!(
            load("huge.png", &huge),
            Err(ImageError::TooLarge { .. })
        ));
        let wide = png(MAX_IMAGE_SIDE + 1, 10);
        assert!(matches!(
            load("wide.png", &wide),
            Err(ImageError::TooManyPixels { .. })
        ));
        assert!(
            load("wide.png", &wide)
                .unwrap_err()
                .to_string()
                .contains("8000")
        );
    }

    #[test]
    fn the_read_text_names_the_mime_and_says_when_provider_scaling_applies() {
        let small = load("a.png", &png(1200, 800)).unwrap();
        let text = read_description(&small, true);
        assert!(text.starts_with("Read image file [image/png] 1200×800"));
        assert!(!text.contains("scales"));
        assert!(!text.contains("does not support"));

        let long = load("b.png", &png(2000, 100)).unwrap();
        assert!(read_description(&long, true).contains("scales it down"));

        let blind = load("c.png", &png(1200, 800)).unwrap();
        let text = read_description(&blind, false);
        assert!(
            text.contains("[Current model does not support images."),
            "{text}"
        );
        assert!(!text.contains("scales"));
    }

    #[test]
    fn an_image_only_message_has_no_empty_text_part() {
        let image = load("shot.png", &png(10, 10)).unwrap();
        let message = message("  ", std::slice::from_ref(&image));
        let radian_core::Message::User { content, .. } = message else {
            panic!("a user message");
        };
        assert_eq!(content.len(), 1, "{content:?}");
        assert!(matches!(content[0], ContentPart::Image { .. }));
    }

    #[test]
    fn base64_is_standard_with_padding() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }
}
