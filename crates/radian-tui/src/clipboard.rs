//! Reading an image from the system clipboard (docs/backlog/images.md Q2/Q7; ADR 0028 R5).
//!
//! The terminal handles `Cmd-V`/`Ctrl-V` itself and sends nothing when the clipboard holds an
//! image, so the TUI reads the clipboard on its own. `arboard` returns RGBA pixels; they are
//! encoded to PNG **in memory** and go through the one image pipeline
//! ([`radian_coding::image::load`]), so the magic number, the 5 MiB and the 8000 px limits are
//! exactly the ones `@`, a dropped path and `read` use. Nothing is written to disk and no path
//! enters the composer: the model receives the image, not a file name.
//!
//! The read is blocking, so it happens in the event loop ([`crate::app::Effect::ReadClipboard`])
//! and answers [`crate::app::Input::ClipboardRead`], never inside `App::update`. The reader is a
//! small trait: a test injects pixels without a real clipboard, and the system implementation
//! is `arboard` (macOS NSPasteboard, Linux X11 and Wayland, Windows).

use std::io::Cursor;
use std::sync::Arc;

use radian_coding::LoadedImage;

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
    match encode_png(width, height, &pixels) {
        Ok(png) => match radian_coding::image::load(CLIPBOARD_NAME, &png) {
            Ok(image) => Outcome::Image(image),
            Err(error) => Outcome::Unusable(error.to_string()),
        },
        Err(reason) => Outcome::Unusable(reason),
    }
}

/// RGBA pixels → PNG bytes, in memory. `image` is already the transcript's decoder, so the
/// clipboard adds no second encoder.
fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| format!("{width}×{height} px is too large"))?;
    if rgba.len() != expected {
        return Err("the clipboard pixels are not RGBA for their size".to_owned());
    }
    let buffer = image::RgbaImage::from_raw(width, height, rgba.to_vec())
        .ok_or_else(|| "the clipboard image size does not match its pixels".to_owned())?;
    let mut out = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(buffer)
        .write_to(&mut out, image::ImageFormat::Png)
        .map_err(|error| format!("the clipboard image cannot be encoded as PNG: {error}"))?;
    Ok(out.into_inner())
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
