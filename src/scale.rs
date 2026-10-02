//! Moving pictures between layouts and sizes on the CPU — the small pure-Rust
//! scaler a transcode needs where there is no FFmpeg to lean on.
//!
//! Playback never comes here: the renderer samples whatever the decoder made,
//! at whatever size, in the shader. A transcode does — an encoder takes one
//! layout (rav1e planar 4:2:0, VideoToolbox NV12) at one size, and an import's
//! proxy is a fraction of the original's.
//!
//! Everything here is 8-bit 4:2:0, which is all §15's output spec allows.
//! Shrinking averages the source pixels each output pixel covers (an area, or
//! box, filter): no ringing, no aliasing worth the name at the ratios a proxy
//! uses, and cheap.

use crate::error::{Error, Result};
use crate::frame::{Frame, PixelFormat};

/// A frame as planar 4:2:0 — what rav1e takes — tightly packed.
///
/// # Errors
///
/// [`Error::Unsupported`] for anything that is not 8-bit 4:2:0 already: 10-bit,
/// 4:2:2 and 4:4:4, and packed RGB, none of which a decoder here produces for
/// a film.
pub fn to_i420(frame: &Frame) -> Result<Frame> {
    let (width, height) = (frame.width(), frame.height());
    let (chroma_width, chroma_height) = chroma_size(width, height);

    let mut data = Vec::with_capacity(i420_size(width, height));

    match frame.format() {
        PixelFormat::I420 => {
            for plane in 0..3 {
                let rows = if plane == 0 { height } else { chroma_height };
                for row in 0..rows {
                    data.extend_from_slice(row_of(frame, plane, row)?);
                }
            }
        }

        PixelFormat::Nv12 => {
            for row in 0..height {
                data.extend_from_slice(row_of(frame, 0, row)?);
            }

            // Interleaved UV into U then V.
            let mut v = Vec::with_capacity(chroma_width as usize * chroma_height as usize);
            for row in 0..chroma_height {
                let pairs = row_of(frame, 1, row)?;
                data.extend(pairs.chunks_exact(2).map(|pair| pair[0]));
                v.extend(pairs.chunks_exact(2).map(|pair| pair[1]));
            }
            data.extend_from_slice(&v);
        }

        other => return Err(not_420(other)),
    }

    Frame::packed(
        width,
        height,
        PixelFormat::I420,
        frame.color(),
        frame.pts(),
        data,
    )
}

/// A frame as NV12 — what VideoToolbox's encoder takes — tightly packed.
///
/// # Errors
///
/// As [`to_i420`].
pub fn to_nv12(frame: &Frame) -> Result<Frame> {
    let (width, height) = (frame.width(), frame.height());
    let (_, chroma_height) = chroma_size(width, height);

    let mut data = Vec::with_capacity(i420_size(width, height));

    match frame.format() {
        PixelFormat::Nv12 => {
            for plane in 0..2 {
                let rows = if plane == 0 { height } else { chroma_height };
                for row in 0..rows {
                    data.extend_from_slice(row_of(frame, plane, row)?);
                }
            }
        }

        PixelFormat::I420 => {
            for row in 0..height {
                data.extend_from_slice(row_of(frame, 0, row)?);
            }

            for row in 0..chroma_height {
                let (u, v) = (row_of(frame, 1, row)?, row_of(frame, 2, row)?);
                for (u, v) in u.iter().zip(v) {
                    data.push(*u);
                    data.push(*v);
                }
            }
        }

        other => return Err(not_420(other)),
    }

    Frame::packed(
        width,
        height,
        PixelFormat::Nv12,
        frame.color(),
        frame.pts(),
        data,
    )
}

/// A frame shrunk to `width` × `height` by averaging, in the same layout.
///
/// Meant for shrinking; asked to grow, each output pixel takes the nearest
/// source pixel, which is honest if not pretty. Odd sizes are allowed — the
/// chroma planes round up, as the layout says.
///
/// # Errors
///
/// [`Error::BadFrame`] for a size with no pixels; as [`to_i420`] for a layout
/// that is not 8-bit 4:2:0.
pub fn resize(frame: &Frame, width: u32, height: u32) -> Result<Frame> {
    if width == 0 || height == 0 {
        return Err(Error::BadFrame {
            reason: format!("cannot scale a picture to {width}×{height}"),
        });
    }

    if (width, height) == (frame.width(), frame.height()) {
        return Ok(frame.clone());
    }

    let format = frame.format();
    let planar = to_i420(frame)?;

    let (from_width, from_height) = (planar.width(), planar.height());
    let (from_chroma_width, from_chroma_height) = chroma_size(from_width, from_height);
    let (chroma_width, chroma_height) = chroma_size(width, height);

    let mut data = Vec::with_capacity(i420_size(width, height));

    let sizes = [
        ((from_width, from_height), (width, height)),
        (
            (from_chroma_width, from_chroma_height),
            (chroma_width, chroma_height),
        ),
        (
            (from_chroma_width, from_chroma_height),
            (chroma_width, chroma_height),
        ),
    ];

    for (plane, (from, to)) in sizes.into_iter().enumerate() {
        let source = planar.plane_data(plane).ok_or_else(|| Error::BadFrame {
            reason: format!("plane {plane} is missing"),
        })?;
        let stride = planar.planes()[plane].stride;

        shrink_plane(source, stride, from, to, &mut data);
    }

    let scaled = Frame::packed(
        width,
        height,
        PixelFormat::I420,
        frame.color(),
        frame.pts(),
        data,
    )?;

    match format {
        PixelFormat::Nv12 => to_nv12(&scaled),
        _ => Ok(scaled),
    }
}

/// The largest size no bigger than `max_width` × `max_height` with the same
/// shape as `width` × `height`, in even numbers — 4:2:0 chroma needs pairs,
/// and several encoders refuse odd sizes outright. Never larger than the
/// original.
pub fn fit_within(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    let scale = (f64::from(max_width) / f64::from(width.max(1)))
        .min(f64::from(max_height) / f64::from(height.max(1)))
        .min(1.0);

    (even(f64::from(width) * scale), even(f64::from(height) * scale))
}

/// Rounded down to an even number, and never below two.
pub(crate) fn even(value: f64) -> u32 {
    ((value.max(2.0) as u32) & !1).max(2)
}

/// One plane, averaged down (or nearest-picked up) into `output`.
fn shrink_plane(
    source: &[u8],
    stride: usize,
    (from_width, from_height): (u32, u32),
    (to_width, to_height): (u32, u32),
    output: &mut Vec<u8>,
) {
    // Which source columns each output column covers, worked out once.
    let spans = |from: u32, to: u32| -> Vec<(usize, usize)> {
        (0..to)
            .map(|index| {
                let start = (u64::from(index) * u64::from(from) / u64::from(to)) as usize;
                let end = (u64::from(index + 1) * u64::from(from) / u64::from(to)) as usize;
                (start, end.max(start + 1).min(from as usize))
            })
            .collect()
    };

    let columns = spans(from_width, to_width);
    let rows = spans(from_height, to_height);
    let mut sums = vec![0_u32; to_width as usize];

    for &(top, bottom) in &rows {
        sums.iter_mut().for_each(|sum| *sum = 0);

        for row in top..bottom {
            let line = &source[row * stride..row * stride + from_width as usize];
            for (sum, &(left, right)) in sums.iter_mut().zip(&columns) {
                *sum += line[left..right].iter().map(|&sample| u32::from(sample)).sum::<u32>();
            }
        }

        let height = (bottom - top) as u32;
        for (sum, &(left, right)) in sums.iter().zip(&columns) {
            let count = height * (right - left) as u32;
            // Rounded to nearest rather than truncated, or every shrink would
            // darken the picture by half a code value.
            output.push(((sum + count / 2) / count) as u8);
        }
    }
}

fn row_of(frame: &Frame, plane: usize, row: u32) -> Result<&[u8]> {
    frame.row(plane, row).ok_or_else(|| Error::BadFrame {
        reason: format!("plane {plane} has no row {row}"),
    })
}

fn chroma_size(width: u32, height: u32) -> (u32, u32) {
    (width.div_ceil(2), height.div_ceil(2))
}

fn i420_size(width: u32, height: u32) -> usize {
    let (chroma_width, chroma_height) = chroma_size(width, height);
    width as usize * height as usize + 2 * chroma_width as usize * chroma_height as usize
}

fn not_420(format: PixelFormat) -> Error {
    Error::unsupported(format!(
        "{format:?} is not 8-bit 4:2:0; vtome encodes only that (planning/TODO.md §15)"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::ColorSpace;
    use std::time::Duration;

    /// An NV12 frame whose luma is its column number and whose chroma pairs
    /// are (U, V) = (row, 200 − row), so every sample says where it came from.
    fn numbered_nv12(width: u32, height: u32) -> Frame {
        let mut data = Vec::new();
        for _ in 0..height {
            data.extend((0..width).map(|x| x as u8));
        }
        for row in 0..height.div_ceil(2) {
            for _ in 0..width.div_ceil(2) {
                data.push(row as u8);
                data.push(200 - row as u8);
            }
        }

        Frame::packed(
            width,
            height,
            PixelFormat::Nv12,
            ColorSpace::default(),
            Duration::from_millis(40),
            data,
        )
        .unwrap()
    }

    #[test]
    fn nv12_and_i420_convert_both_ways_without_loss() {
        let original = numbered_nv12(6, 4);

        let planar = to_i420(&original).unwrap();
        assert_eq!(planar.format(), PixelFormat::I420);
        assert_eq!(planar.row(1, 1).unwrap(), [1, 1, 1], "U is the row");
        assert_eq!(planar.row(2, 1).unwrap(), [199, 199, 199], "V is 200 − row");
        assert_eq!(planar.pts(), original.pts());

        let back = to_nv12(&planar).unwrap();
        assert_eq!(back.data(), original.data());
    }

    /// Two columns averaged into one: 0 and 1 make 0.5, rounded to 1; a
    /// uniform plane stays exactly what it was.
    #[test]
    fn halving_averages_each_pair() {
        let original = numbered_nv12(8, 4);
        let half = resize(&original, 4, 2).unwrap();

        assert_eq!(half.format(), PixelFormat::Nv12, "the layout is kept");
        assert_eq!((half.width(), half.height()), (4, 2));
        assert_eq!(half.row(0, 0).unwrap(), [1, 3, 5, 7]);

        // Chroma rows 0 and 1 average to 0.5 → 1, and 200/199 to 199.5 → 200.
        assert_eq!(half.row(1, 0).unwrap(), [1, 200, 1, 200]);
    }

    #[test]
    fn odd_sizes_keep_their_last_chroma_column() {
        let original = numbered_nv12(7, 5);
        let planar = to_i420(&original).unwrap();

        assert_eq!(planar.row(1, 2).unwrap().len(), 4, "7 wide is 4 chroma wide");

        let shrunk = resize(&original, 3, 3).unwrap();
        assert_eq!((shrunk.width(), shrunk.height()), (3, 3));
    }

    #[test]
    fn fitting_keeps_the_shape_in_even_numbers_and_never_grows() {
        assert_eq!(fit_within(1920, 1080, 640, 640), (640, 360));
        assert_eq!(fit_within(1080, 1920, 640, 640), (360, 640));
        assert_eq!(fit_within(1000, 563, 640, 640), (640, 360), "even, rounded down");
        assert_eq!(fit_within(320, 240, 640, 640), (320, 240));
    }

    #[test]
    fn only_eight_bit_four_two_zero_is_converted() {
        let rgba = Frame::packed(
            2,
            2,
            PixelFormat::Rgba8,
            ColorSpace::srgb(),
            Duration::ZERO,
            vec![0; 16],
        )
        .unwrap();

        assert!(matches!(to_i420(&rgba), Err(Error::Unsupported { .. })));
        assert!(resize(&rgba, 1, 1).is_err());
        assert!(resize(&numbered_nv12(4, 4), 0, 2).is_err());
    }
}
