//! Still images, down the same pipe as video.
//!
//! A photograph on a trapezoid should take the same code as a film on one, so a
//! loaded image becomes a [`Frame`] — full-range, identity matrix, one picture
//! at time zero — and everything downstream stops caring which it was.
//!
//! ```no_run
//! let frame = vtome::load_image("poster.png")?;
//!
//! println!("{}×{}", frame.width(), frame.height());
//! # Ok::<(), vtome::Error>(())
//! ```

use std::path::Path;
use std::time::Duration;

use crate::color::ColorSpace;
use crate::error::{Error, Result};
use crate::frame::{Frame, PixelFormat};

/// The largest picture that will be loaded without being asked twice.
///
/// A 16-bit TIFF from a scanner is easily 30 000 pixels on a side, which is
/// past what most GPUs will hold in one texture and past what a surprised
/// caller wants allocated. [`load_image_limited`] raises it deliberately.
pub const DEFAULT_MAX_PIXELS: u64 = 16_384 * 16_384;

/// Loads an image as an RGBA frame.
///
/// # Errors
///
/// [`Error::Io`] if it cannot be read, [`Error::Unsupported`] if the format is
/// not one the `image` feature was built with or the picture is past
/// [`DEFAULT_MAX_PIXELS`].
pub fn load_image(path: impl AsRef<Path>) -> Result<Frame> {
    load_image_limited(path, DEFAULT_MAX_PIXELS)
}

/// [`load_image`] with the ceiling set by the caller.
///
/// # Errors
///
/// As [`load_image`], against `max_pixels` instead.
pub fn load_image_limited(path: impl AsRef<Path>, max_pixels: u64) -> Result<Frame> {
    let path = path.as_ref();

    let reader = image::ImageReader::open(path)
        .map_err(|error| Error::io(path, error))?
        .with_guessed_format()
        .map_err(|error| Error::io(path, error))?;

    // The dimensions come out of the header, so an absurd picture is refused
    // before its pixels are allocated rather than after.
    if let Ok((width, height)) = reader.into_dimensions() {
        let pixels = u64::from(width) * u64::from(height);

        if pixels > max_pixels {
            return Err(Error::unsupported(format!(
                "{} is {width}×{height} — {pixels} pixels, past the {max_pixels} ceiling",
                path.display()
            )));
        }
    }

    // Re-open: `into_dimensions` consumes the reader, and reading the header
    // twice is cheaper than holding a decoded image that turns out to be too
    // large.
    let decoded = image::ImageReader::open(path)
        .map_err(|error| Error::io(path, error))?
        .with_guessed_format()
        .map_err(|error| Error::io(path, error))?
        .decode()
        .map_err(|error| Error::unsupported(format!("{}: {error}", path.display())))?;

    frame_from_image(decoded)
}

/// Decodes an image already in memory — from a bundle, or a network fetch.
///
/// # Errors
///
/// [`Error::Unsupported`] if the bytes are not an image this build reads.
pub fn load_image_bytes(bytes: &[u8]) -> Result<Frame> {
    let decoded = image::load_from_memory(bytes)
        .map_err(|error| Error::unsupported(format!("not a readable image: {error}")))?;

    frame_from_image(decoded)
}

/// The common tail: RGBA8, full range, identity matrix, at time zero.
fn frame_from_image(image: image::DynamicImage) -> Result<Frame> {
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();

    Frame::packed(
        width,
        height,
        PixelFormat::Rgba8,
        // sRGB and full range: a still is not video and does not carry video's
        // limited-range pedestal. Getting this wrong is what makes a PNG look
        // washed out next to the same picture in a browser.
        ColorSpace::srgb(),
        Duration::ZERO,
        rgba.into_raw(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2×2 PNG, written by the same library that reads it — a fixture that
    /// cannot drift from what the decoder expects.
    fn png(path: &Path, width: u32, height: u32) {
        let mut buffer = image::RgbaImage::new(width, height);

        for (x, y, pixel) in buffer.enumerate_pixels_mut() {
            *pixel = image::Rgba([(x * 60) as u8, (y * 60) as u8, 128, 255]);
        }

        buffer.save(path).unwrap();
    }

    #[test]
    fn an_image_arrives_as_an_rgba_frame() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("swatch.png");
        png(&path, 4, 3);

        let frame = load_image(&path).unwrap();

        assert_eq!((frame.width(), frame.height()), (4, 3));
        assert_eq!(frame.format(), PixelFormat::Rgba8);
        assert_eq!(frame.byte_len(), 4 * 3 * 4);
        assert_eq!(frame.pts(), Duration::ZERO);
    }

    /// Stills are full-range sRGB; treating them as limited-range video is the
    /// washed-out-photo bug.
    #[test]
    fn a_still_is_full_range_and_needs_no_matrix() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("swatch.png");
        png(&path, 2, 2);

        let frame = load_image(&path).unwrap();

        assert_eq!(frame.color(), ColorSpace::srgb());
        assert_eq!(frame.color().range, crate::color::Range::Full);
    }

    #[test]
    fn the_pixels_survive_the_trip() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("swatch.png");
        png(&path, 2, 2);

        let frame = load_image(&path).unwrap();
        let row = frame.row(0, 0).unwrap();

        assert_eq!(&row[..4], &[0, 0, 128, 255]);
        assert_eq!(&row[4..8], &[60, 0, 128, 255]);
    }

    #[test]
    fn an_image_in_memory_loads_the_same_way() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("swatch.png");
        png(&path, 3, 3);

        let bytes = std::fs::read(&path).unwrap();
        let frame = load_image_bytes(&bytes).unwrap();

        assert_eq!((frame.width(), frame.height()), (3, 3));
    }

    /// The ceiling is checked against the header, so it costs nothing and
    /// happens before the allocation it is protecting against.
    #[test]
    fn a_picture_past_the_ceiling_is_refused_from_its_header() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("swatch.png");
        png(&path, 64, 64);

        let Err(Error::Unsupported { what }) = load_image_limited(&path, 100) else {
            panic!("64×64 is 4096 pixels, past a ceiling of 100");
        };

        assert!(what.contains("ceiling"), "{what}");
    }

    #[test]
    fn a_file_that_is_not_an_image_is_refused_rather_than_guessed_at() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("notes.txt");
        std::fs::write(&path, b"not a picture").unwrap();

        assert!(load_image(&path).is_err());
        assert!(load_image_bytes(b"not a picture").is_err());
    }

    #[test]
    fn a_missing_file_says_which_file() {
        let Err(Error::Io { path, .. }) = load_image("definitely/not/here.png") else {
            panic!("that file does not exist");
        };

        assert!(path.ends_with("here.png"));
    }
}
