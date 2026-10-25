//! Reading an image from the system clipboard (docs/backlog/images.md Q2/Q7; ADR 0028 R5).
//!
//! The terminal handles `Cmd-V`/`Ctrl-V` itself and sends nothing when the clipboard holds an
//! image, so the TUI reads the clipboard on its own. `arboard` returns RGBA pixels; they are
//! encoded to PNG **in memory** and go through the one image pipeline
//! ([`radian_coding::image::load`]), so the magic number, the 5 MiB and the 8000 px limits are
//! exactly the ones `@`, a dropped path and `read` use. Nothing is written to disk and no path
//! enters the composer: the model receives the image, not a file name.
//!
//! The dimensions are checked on the clipboard's own RGBA shape **before** the pixels are copied
//! or encoded, and the encoder writes through a sink that stops at the shared 5 MiB limit, so a
//! huge paste is refused without first building a 256 MiB pixel copy and a huge PNG. Accepted
//! bytes still go through [`radian_coding::image::load`]: it is the one pipeline's last word.
//!
//! The read is blocking, so it happens in the event loop ([`crate::app::Effect::ReadClipboard`])
//! and answers [`crate::app::Input::ClipboardRead`], never inside `App::update`. The reader is a
//! small trait: a test injects pixels without a real clipboard, and the system implementation
//! is `arboard` (macOS NSPasteboard, Linux X11 and Wayland, Windows).

use std::sync::Arc;

use image::ImageEncoder;
use radian_coding::{ImageError, LoadedImage, MAX_IMAGE_BYTES, MAX_IMAGE_SIDE};

/// The display source name of every clipboard image: `[image: clipboard, W×H, N KB]`.
pub const CLIPBOARD_NAME: &str = "clipboard";

/// What a clipboard holds, as the reader reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    /// RGBA pixels, eight bits per channel, row-major, `width * height * 4` bytes.
    Rgba {
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    },
    /// No image. An empty clipboard and a text-only one both end here: Ctrl-V does what it did
    /// before (nothing in the composer) and no attachment appears.
    NoImage,
}

/// Reads the system clipboard. [`App`](crate::app::App) holds one; a test replaces it with an
/// injected image, and no test ever touches the real clipboard.
pub trait Reader: Send + Sync {
    fn read(&self) -> Content;
}

/// The real reader: `arboard`.
pub struct System;

impl Reader for System {
    fn read(&self) -> Content {
        let Ok(mut clipboard) = arboard::Clipboard::new() else {
            return Content::NoImage;
        };
        let Ok(image) = clipboard.get_image() else {
            return Content::NoImage;
        };
        let (Ok(width), Ok(height)) = (u32::try_from(image.width), u32::try_from(image.height))
        else {
            return Content::NoImage;
        };
        let Some(expected) = (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
        else {
            return Content::NoImage;
        };
        if image.bytes.len() != expected {
            return Content::NoImage;
        }
        Content::Rgba {
            width,
            height,
            pixels: image.bytes.into_owned(),
        }
    }
}

/// The default reader: the system clipboard.
pub fn system() -> Arc<dyn Reader> {
    Arc::new(System)
}

/// What reading the clipboard produced, after the shared image pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// A PNG that passed every check, ready to join the composer attachments.
    Image(LoadedImage),
    /// No image (text only, or empty): nothing image-related happens.
    NoImage,
    /// An image over a limit, or one whose bytes cannot be read: the unusable-attachment
    /// reason, the same shape `@` reports.
    Unusable(String),
}

/// Reads once and turns an image into the shared image part. Called by the event loop.
pub fn read(reader: &dyn Reader) -> Outcome {
    let Content::Rgba {
        width,
        height,
        pixels,
    } = reader.read()
    else {
        return Outcome::NoImage;
    };
    // The per-side limit is checked on the clipboard's own dimensions, before the pixels are
    // moved, copied or encoded: an over-limit paste must not reach the encoder at all. The
    // reason is the shared pipeline's, so the notice names the limit exactly as `@` does.
    if width > MAX_IMAGE_SIDE || height > MAX_IMAGE_SIDE {
        return Outcome::Unusable(ImageError::TooManyPixels { width, height }.to_string());
    }
    match encode_png(width, height, pixels) {
        Ok(png) => match radian_coding::image::load(CLIPBOARD_NAME, &png) {
            Ok(image) => Outcome::Image(image),
            Err(error) => Outcome::Unusable(error.to_string()),
        },
        Err(reason) => Outcome::Unusable(reason),
    }
}

/// RGBA pixels → PNG bytes, in memory, bounded by the shared byte limit. `image` is already the
/// transcript's decoder, so the clipboard adds no second encoder. The pixels are taken by value
/// (the reader's buffer is reused, never cloned) and the encoder writes through [`Bounded`], so
/// an image that cannot compress under the limit is refused while it is encoded.
fn encode_png(width: u32, height: u32, rgba: Vec<u8>) -> Result<Vec<u8>, String> {
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| format!("{width}×{height} px is too large"))?;
    if rgba.len() != expected {
        return Err("the clipboard pixels are not RGBA for their size".to_owned());
    }
    let mut sink = Bounded::default();
    let encoded = image::codecs::png::PngEncoder::new(&mut sink).write_image(
        &rgba,
        width,
        height,
        image::ExtendedColorType::Rgba8,
    );
    if let Err(error) = encoded {
        return Err(if sink.over {
            format!(
                "the encoded PNG is over the {} MiB image limit",
                MAX_IMAGE_BYTES / (1024 * 1024)
            )
        } else {
            format!("the clipboard image cannot be encoded as PNG: {error}")
        });
    }
    Ok(sink.bytes)
}

/// Collects an encoded PNG up to the shared byte limit. The PNG encoder writes in chunks, so a
/// paste that cannot compress under 5 MiB stops here instead of building a buffer that
/// [`radian_coding::image::load`] would reject only after the whole encode.
#[derive(Default)]
struct Bounded {
    bytes: Vec<u8>,
    /// Whether a write crossed the limit (the encoder only reports an opaque IO error).
    over: bool,
}

impl std::io::Write for Bounded {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let limit = usize::try_from(MAX_IMAGE_BYTES).unwrap_or(usize::MAX);
        if self.bytes.len().saturating_add(buf.len()) > limit {
            self.over = true;
            return Err(std::io::Error::other("over the image byte limit"));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed RGBA image; no real clipboard is touched.
    struct Fake(Content);

    impl Reader for Fake {
        fn read(&self) -> Content {
            self.0.clone()
        }
    }

    fn rgba(width: u32, height: u32, pixel: [u8; 4]) -> Content {
        let pixels = pixel
            .iter()
            .copied()
            .cycle()
            .take(width as usize * height as usize * 4)
            .collect();
        Content::Rgba {
            width,
            height,
            pixels,
        }
    }

    /// RGBA pixels that do not compress: a PNG of them is larger than the raw buffer, so it
    /// crosses the 5 MiB limit while it is encoded.
    fn noise(width: u32, height: u32) -> Content {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
        for _ in 0..width as usize * height as usize {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            pixels.extend_from_slice(&state.to_le_bytes()[..4]);
        }
        Content::Rgba {
            width,
            height,
            pixels,
        }
    }

    #[test]
    fn an_rgba_image_becomes_a_png_loaded_image() {
        let outcome = read(&Fake(rgba(3, 2, [10, 20, 30, 255])));
        let Outcome::Image(image) = outcome else {
            panic!("an image: {outcome:?}");
        };
        assert_eq!(image.name, CLIPBOARD_NAME);
        assert_eq!(image.mime, "image/png");
        assert_eq!((image.width, image.height), (3, 2));
        assert!(image.data.starts_with("iVBORw0KGgo"), "{}", image.data);
    }

    #[test]
    fn no_image_is_no_image() {
        assert_eq!(read(&Fake(Content::NoImage)), Outcome::NoImage);
    }

    #[test]
    fn an_over_limit_image_is_unusable() {
        let wide = rgba(radian_coding::MAX_IMAGE_SIDE + 1, 1, [0, 0, 0, 255]);
        let outcome = read(&Fake(wide));
        assert!(
            matches!(&outcome, Outcome::Unusable(reason) if reason.contains("8000")),
            "{outcome:?}"
        );
    }

    /// The per-side limit is decided from the clipboard's own shape, before the pixel buffer is
    /// looked at (and so before any copy): pixels that do not even match their size still get
    /// the 8000 px reason, not the RGBA-shape one.
    #[test]
    fn an_over_limit_dimension_is_refused_before_the_pixels_are_touched() {
        let outcome = read(&Fake(Content::Rgba {
            width: radian_coding::MAX_IMAGE_SIDE + 1,
            height: 1,
            pixels: Vec::new(),
        }));
        assert!(
            matches!(&outcome, Outcome::Unusable(reason) if reason.contains("8000")),
            "the dimension limit is checked first: {outcome:?}"
        );
    }

    /// An image within the side limit but too large to compress under the byte limit is refused
    /// by the bounded encoder, before a PNG of it could be built and rejected afterwards.
    #[test]
    fn an_incompressible_image_is_refused_while_it_is_encoded() {
        let outcome = read(&Fake(noise(1400, 1400)));
        assert!(
            matches!(&outcome, Outcome::Unusable(reason) if reason.contains("5 MiB") && reason.contains("encoded")),
            "{outcome:?}"
        );
    }

    #[test]
    fn pixels_that_are_not_rgba_are_refused() {
        let outcome = read(&Fake(Content::Rgba {
            width: 2,
            height: 2,
            pixels: vec![0; 3],
        }));
        assert!(matches!(outcome, Outcome::Unusable(_)), "{outcome:?}");
    }
}
