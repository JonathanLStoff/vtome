//! One decoded picture, and the pool that stops decoding from allocating.
//!
//! Frames stay in whatever the decoder produced — usually planar YUV with the
//! chroma at half resolution — all the way to the shader. Converting to RGBA on
//! the CPU would triple the bytes crossing the bus and burn a core doing what
//! the GPU does for free; see [`crate::color`].

use std::sync::Arc;
use std::time::Duration;

use crate::color::ColorSpace;
use crate::error::{Error, Result};

/// How the samples are laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PixelFormat {
    /// Planar Y, U, V with chroma at half width and half height. What almost
    /// every consumer video decodes to.
    I420,
    /// Planar, chroma at half width and full height.
    I422,
    /// Planar, chroma at full resolution.
    I444,
    /// Y plane then interleaved UV, half width and half height. What the
    /// hardware decoders hand back.
    Nv12,
    /// NV12 at 10 bits, each sample in the top bits of a 16-bit word.
    P010,
    /// Packed 8-bit RGBA. Still images, and anything already converted.
    Rgba8,
    /// Packed 8-bit BGRA, which is what several platform surfaces want.
    Bgra8,
}

impl PixelFormat {
    /// How many separately-addressed planes there are.
    pub fn plane_count(self) -> usize {
        match self {
            PixelFormat::I420 | PixelFormat::I422 | PixelFormat::I444 => 3,
            PixelFormat::Nv12 | PixelFormat::P010 => 2,
            PixelFormat::Rgba8 | PixelFormat::Bgra8 => 1,
        }
    }

    /// Bits per sample, per component.
    pub fn bit_depth(self) -> u32 {
        match self {
            PixelFormat::P010 => 10,
            _ => 8,
        }
    }

    /// Bytes each sample occupies, which is not the same as its bit depth: a
    /// 10-bit sample is stored in two bytes.
    pub fn bytes_per_sample(self) -> usize {
        match self {
            PixelFormat::P010 => 2,
            _ => 1,
        }
    }

    /// Whether the planes are luma and chroma rather than colour.
    pub fn is_yuv(self) -> bool {
        !matches!(self, PixelFormat::Rgba8 | PixelFormat::Bgra8)
    }

    /// How far chroma is subsampled, as right-shifts of the luma dimensions.
    ///
    /// `(1, 1)` for 4:2:0 — half width, half height. `(0, 0)` for anything with
    /// full-resolution colour.
    pub fn chroma_shift(self) -> (u32, u32) {
        match self {
            PixelFormat::I420 | PixelFormat::Nv12 | PixelFormat::P010 => (1, 1),
            PixelFormat::I422 => (1, 0),
            _ => (0, 0),
        }
    }

    /// The size of one plane in samples, for a picture of this size.
    ///
    /// Chroma dimensions round *up*: a 1921-pixel-wide 4:2:0 picture has 961
    /// chroma columns, not 960, and rounding down loses the last one.
    pub fn plane_dimensions(self, plane: usize, width: u32, height: u32) -> Option<(u32, u32)> {
        if plane >= self.plane_count() {
            return None;
        }

        // Plane 0 is always full resolution; on packed formats it is the only
        // one, and its "width" counts pixels rather than samples.
        if plane == 0 {
            return Some((width, height));
        }

        let (h_shift, v_shift) = self.chroma_shift();
        let chroma_width = width.div_ceil(1 << h_shift);
        let chroma_height = height.div_ceil(1 << v_shift);

        Some((chroma_width, chroma_height))
    }

    /// Samples per pixel within one plane: 4 for packed RGBA, 2 for NV12's
    /// interleaved chroma, 1 for a planar plane.
    pub fn samples_per_pixel(self, plane: usize) -> usize {
        match (self, plane) {
            (PixelFormat::Rgba8 | PixelFormat::Bgra8, _) => 4,
            (PixelFormat::Nv12 | PixelFormat::P010, 1) => 2,
            _ => 1,
        }
    }

    /// The tightest stride for a plane — no padding at all.
    pub fn tight_stride(self, plane: usize, width: u32) -> usize {
        let (plane_width, _) = self.plane_dimensions(plane, width, 1).unwrap_or((width, 1));

        plane_width as usize * self.samples_per_pixel(plane) * self.bytes_per_sample()
    }
}

/// Where one plane sits in a frame's buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plane {
    /// Byte offset from the start of the frame's data.
    pub offset: usize,
    /// Bytes between the start of one row and the start of the next.
    ///
    /// Decoders align rows for their own reasons, so this is almost never the
    /// row's width in bytes. Treating the two as the same is the classic way to
    /// get a picture that shears diagonally.
    pub stride: usize,
}

/// A decoded picture.
///
/// The data is behind an [`Arc`] so a frame can be handed to a renderer, an
/// encoder, and a callback at once without copying — none of them mutate it.
#[derive(Clone, Debug)]
pub struct Frame {
    width: u32,
    height: u32,
    format: PixelFormat,
    color: ColorSpace,
    pts: Duration,
    planes: Vec<Plane>,
    data: Arc<Vec<u8>>,
}

impl Frame {
    /// A frame over `data`, with the planes laid out end to end and no padding.
    ///
    /// # Errors
    ///
    /// [`Error::BadFrame`] if `data` is not the size that layout implies, or
    /// either dimension is zero.
    pub fn packed(
        width: u32,
        height: u32,
        format: PixelFormat,
        color: ColorSpace,
        pts: Duration,
        data: Vec<u8>,
    ) -> Result<Self> {
        let (planes, needed) = packed_layout(format, width, height)?;

        if data.len() < needed {
            return Err(Error::BadFrame {
                reason: format!(
                    "{width}×{height} {format:?} needs {needed} bytes, got {}",
                    data.len()
                ),
            });
        }

        Ok(Frame {
            width,
            height,
            format,
            color,
            pts,
            planes,
            data: Arc::new(data),
        })
    }

    /// A frame whose planes are where the decoder put them.
    ///
    /// # Errors
    ///
    /// [`Error::BadFrame`] if the plane count is wrong for the format, or any
    /// plane's last row runs past the end of `data`.
    pub fn with_planes(
        width: u32,
        height: u32,
        format: PixelFormat,
        color: ColorSpace,
        pts: Duration,
        planes: Vec<Plane>,
        data: Vec<u8>,
    ) -> Result<Self> {
        if width == 0 || height == 0 {
            return Err(Error::BadFrame {
                reason: format!("a {width}×{height} picture has no pixels"),
            });
        }

        if planes.len() != format.plane_count() {
            return Err(Error::BadFrame {
                reason: format!(
                    "{format:?} has {} planes, {} given",
                    format.plane_count(),
                    planes.len()
                ),
            });
        }

        for (index, plane) in planes.iter().enumerate() {
            let (plane_width, plane_height) = format
                .plane_dimensions(index, width, height)
                .expect("plane count was checked");

            let row_bytes =
                plane_width as usize * format.samples_per_pixel(index) * format.bytes_per_sample();

            if plane.stride < row_bytes {
                return Err(Error::BadFrame {
                    reason: format!(
                        "plane {index}: stride {} is shorter than a {row_bytes}-byte row",
                        plane.stride
                    ),
                });
            }

            // The last row needs `row_bytes`, not a whole stride — a decoder is
            // entitled to end the buffer at the end of the picture.
            let span = plane
                .stride
                .checked_mul(plane_height.saturating_sub(1) as usize)
                .and_then(|full| full.checked_add(row_bytes))
                .and_then(|span| span.checked_add(plane.offset))
                .ok_or_else(|| Error::BadFrame {
                    reason: format!("plane {index}: the layout overflows a usize"),
                })?;

            if span > data.len() {
                return Err(Error::BadFrame {
                    reason: format!(
                        "plane {index} runs to byte {span} of a {}-byte buffer",
                        data.len()
                    ),
                });
            }
        }

        Ok(Frame {
            width,
            height,
            format,
            color,
            pts,
            planes,
            data: Arc::new(data),
        })
    }

    /// Width in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The sample layout.
    pub fn format(&self) -> PixelFormat {
        self.format
    }

    /// What the samples mean.
    pub fn color(&self) -> ColorSpace {
        self.color
    }

    /// When this picture is due, from the start of the stream.
    pub fn pts(&self) -> Duration {
        self.pts
    }

    /// Retimes the frame, for a player that offsets a stream against a master
    /// clock. Cheap: the data is shared, not copied.
    pub fn with_pts(&self, pts: Duration) -> Self {
        Frame {
            pts,
            data: Arc::clone(&self.data),
            planes: self.planes.clone(),
            ..*self
        }
    }

    /// Where each plane is.
    pub fn planes(&self) -> &[Plane] {
        &self.planes
    }

    /// One plane's bytes, from its offset to the end of its last row.
    pub fn plane_data(&self, plane: usize) -> Option<&[u8]> {
        let descriptor = self.planes.get(plane)?;
        let (plane_width, plane_height) =
            self.format
                .plane_dimensions(plane, self.width, self.height)?;

        let row_bytes = plane_width as usize
            * self.format.samples_per_pixel(plane)
            * self.format.bytes_per_sample();
        let span = descriptor.stride * (plane_height as usize - 1) + row_bytes;

        self.data.get(descriptor.offset..descriptor.offset + span)
    }

    /// One row of one plane, without the padding a stride may add.
    pub fn row(&self, plane: usize, row: u32) -> Option<&[u8]> {
        let descriptor = self.planes.get(plane)?;
        let (plane_width, plane_height) =
            self.format
                .plane_dimensions(plane, self.width, self.height)?;

        if row >= plane_height {
            return None;
        }

        let row_bytes = plane_width as usize
            * self.format.samples_per_pixel(plane)
            * self.format.bytes_per_sample();
        let start = descriptor.offset + descriptor.stride * row as usize;

        self.data.get(start..start + row_bytes)
    }

    /// The whole buffer, planes and padding alike.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// How many bytes this frame holds, for accounting against a memory
    /// ceiling.
    pub fn byte_len(&self) -> usize {
        self.data.len()
    }
}

/// Plane offsets and total size for a tightly packed frame.
fn packed_layout(format: PixelFormat, width: u32, height: u32) -> Result<(Vec<Plane>, usize)> {
    if width == 0 || height == 0 {
        return Err(Error::BadFrame {
            reason: format!("a {width}×{height} picture has no pixels"),
        });
    }

    let mut planes = Vec::with_capacity(format.plane_count());
    let mut offset = 0_usize;

    for index in 0..format.plane_count() {
        let (plane_width, plane_height) = format
            .plane_dimensions(index, width, height)
            .expect("index is below the plane count");

        let stride =
            plane_width as usize * format.samples_per_pixel(index) * format.bytes_per_sample();

        planes.push(Plane { offset, stride });

        offset = stride
            .checked_mul(plane_height as usize)
            .and_then(|size| size.checked_add(offset))
            .ok_or_else(|| Error::BadFrame {
                reason: format!("{width}×{height} {format:?} does not fit in memory"),
            })?;
    }

    Ok((planes, offset))
}

/// The number of bytes a tightly packed frame of this description needs.
///
/// For sizing a pool, or refusing a resolution before allocating for it.
pub fn packed_size(format: PixelFormat, width: u32, height: u32) -> Result<usize> {
    packed_layout(format, width, height).map(|(_, size)| size)
}

/// Buffers handed back for reuse, so steady-state playback allocates nothing.
///
/// Decoding 24 frames a second means 24 allocations a second of several
/// megabytes each; at 4K that is enough to keep an allocator busy and enough to
/// fragment one. Frames come back here when the renderer is done with them.
#[derive(Debug, Default)]
pub struct FramePool {
    buffers: Vec<Vec<u8>>,
    capacity: usize,
    reused: u64,
    allocated: u64,
}

impl FramePool {
    /// A pool holding at most `capacity` buffers.
    ///
    /// Bounded on purpose: an unbounded pool is a memory leak that happens to
    /// be spelled differently.
    pub fn new(capacity: usize) -> Self {
        FramePool {
            buffers: Vec::with_capacity(capacity),
            capacity,
            reused: 0,
            allocated: 0,
        }
    }

    /// A buffer of at least `size` bytes, cleared to that length.
    pub fn take(&mut self, size: usize) -> Vec<u8> {
        // Any buffer big enough will do; taking the first avoids a scan that
        // would cost more than the allocation it saves.
        if let Some(index) = self
            .buffers
            .iter()
            .position(|buffer| buffer.capacity() >= size)
        {
            let mut buffer = self.buffers.swap_remove(index);
            buffer.clear();
            buffer.resize(size, 0);
            self.reused += 1;

            return buffer;
        }

        self.allocated += 1;

        vec![0; size]
    }

    /// Offers a buffer back. Dropped if the pool is full.
    pub fn give(&mut self, buffer: Vec<u8>) {
        if self.buffers.len() < self.capacity {
            self.buffers.push(buffer);
        }
    }

    /// Takes a frame's buffer back if nothing else still holds it.
    ///
    /// A frame the renderer is still reading is left alone — which is the point
    /// of the [`Arc`]: reuse is an optimisation, never a race.
    pub fn reclaim(&mut self, frame: Frame) {
        if let Ok(buffer) = Arc::try_unwrap(frame.data) {
            self.give(buffer);
        }
    }

    /// How many `take` calls were answered without allocating, and how many
    /// were not. Steady-state playback should stop adding to the second.
    pub fn statistics(&self) -> (u64, u64) {
        (self.reused, self.allocated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_of(format: PixelFormat, width: u32, height: u32) -> Frame {
        let size = packed_size(format, width, height).unwrap();

        Frame::packed(
            width,
            height,
            format,
            ColorSpace::default(),
            Duration::ZERO,
            vec![0; size],
        )
        .unwrap()
    }

    #[test]
    fn i420_is_one_and_a_half_bytes_a_pixel() {
        let size = packed_size(PixelFormat::I420, 1920, 1080).unwrap();

        assert_eq!(size, 1920 * 1080 * 3 / 2);
    }

    /// The off-by-one that eats the right-hand column: 4:2:0 chroma for an odd
    /// width has to round up.
    #[test]
    fn odd_dimensions_round_chroma_up() {
        let format = PixelFormat::I420;

        assert_eq!(format.plane_dimensions(1, 1921, 1081), Some((961, 541)));

        let size = packed_size(format, 1921, 1081).unwrap();
        assert_eq!(size, 1921 * 1081 + 2 * (961 * 541));
    }

    #[test]
    fn nv12_has_two_planes_and_interleaved_chroma() {
        assert_eq!(PixelFormat::Nv12.plane_count(), 2);
        assert_eq!(PixelFormat::Nv12.samples_per_pixel(1), 2);

        let size = packed_size(PixelFormat::Nv12, 640, 480).unwrap();
        assert_eq!(size, 640 * 480 * 3 / 2);
    }

    #[test]
    fn p010_takes_two_bytes_a_sample() {
        assert_eq!(PixelFormat::P010.bit_depth(), 10);
        assert_eq!(
            packed_size(PixelFormat::P010, 640, 480).unwrap(),
            640 * 480 * 3
        );
    }

    #[test]
    fn a_frame_too_small_for_its_dimensions_is_refused() {
        let error = Frame::packed(
            1920,
            1080,
            PixelFormat::I420,
            ColorSpace::default(),
            Duration::ZERO,
            vec![0; 100],
        );

        assert!(matches!(error, Err(Error::BadFrame { .. })));
    }

    #[test]
    fn a_zero_sized_picture_is_refused() {
        assert!(matches!(
            packed_size(PixelFormat::I420, 0, 1080),
            Err(Error::BadFrame { .. })
        ));
    }

    /// Strides are what decoders actually hand back, and the last row is
    /// allowed to stop at the end of the picture rather than the end of a
    /// stride.
    #[test]
    fn padded_strides_are_accepted_and_the_last_row_need_not_be_padded() {
        let width = 100_u32;
        let height = 10_u32;
        let stride = 128_usize;

        let luma = stride * (height as usize - 1) + width as usize;
        let chroma_rows = height.div_ceil(2) as usize;
        let chroma = stride * (chroma_rows - 1) + width.div_ceil(2) as usize;

        let planes = vec![
            Plane { offset: 0, stride },
            Plane {
                offset: luma,
                stride,
            },
            Plane {
                offset: luma + chroma,
                stride,
            },
        ];

        let total = luma + 2 * chroma;

        let frame = Frame::with_planes(
            width,
            height,
            PixelFormat::I420,
            ColorSpace::default(),
            Duration::ZERO,
            planes,
            vec![0; total],
        )
        .unwrap();

        assert_eq!(frame.row(0, 9).unwrap().len(), width as usize);
        assert!(frame.row(0, 10).is_none(), "there is no eleventh row");
    }

    #[test]
    fn a_stride_shorter_than_a_row_is_refused() {
        let planes = vec![
            Plane {
                offset: 0,
                stride: 10,
            },
            Plane {
                offset: 0,
                stride: 10,
            },
            Plane {
                offset: 0,
                stride: 10,
            },
        ];

        let error = Frame::with_planes(
            100,
            10,
            PixelFormat::I420,
            ColorSpace::default(),
            Duration::ZERO,
            planes,
            vec![0; 10_000],
        );

        let Err(Error::BadFrame { reason }) = error else {
            panic!("a 10-byte stride cannot hold a 100-byte row");
        };

        assert!(reason.contains("stride"), "{reason}");
    }

    /// The hostile case: a header claiming a stride that walks off the end of
    /// the buffer. Caught here rather than in a decoder's memcpy.
    #[test]
    fn a_plane_that_runs_past_the_buffer_is_refused() {
        let planes = vec![
            Plane {
                offset: 0,
                stride: 1024,
            },
            Plane {
                offset: 0,
                stride: 1024,
            },
            Plane {
                offset: 0,
                stride: 1024,
            },
        ];

        let error = Frame::with_planes(
            100,
            100,
            PixelFormat::I420,
            ColorSpace::default(),
            Duration::ZERO,
            planes,
            vec![0; 1000],
        );

        assert!(matches!(error, Err(Error::BadFrame { .. })));
    }

    #[test]
    fn retiming_shares_the_pixels() {
        let frame = frame_of(PixelFormat::I420, 64, 64);
        let moved = frame.with_pts(Duration::from_millis(500));

        assert_eq!(moved.pts(), Duration::from_millis(500));
        assert_eq!(frame.pts(), Duration::ZERO);
        assert!(
            Arc::ptr_eq(&frame.data, &moved.data),
            "retiming copied the picture"
        );
    }

    #[test]
    fn the_pool_stops_allocating_once_it_is_warm() {
        let mut pool = FramePool::new(4);
        let size = packed_size(PixelFormat::I420, 320, 240).unwrap();

        for _ in 0..10 {
            let buffer = pool.take(size);
            pool.give(buffer);
        }

        let (reused, allocated) = pool.statistics();
        assert_eq!(allocated, 1, "one allocation should have served all ten");
        assert_eq!(reused, 9);
    }

    #[test]
    fn the_pool_will_not_take_a_buffer_still_being_read() {
        let mut pool = FramePool::new(4);
        let frame = frame_of(PixelFormat::I420, 64, 64);
        let still_held = frame.clone();

        pool.reclaim(frame);
        assert_eq!(pool.buffers.len(), 0, "that buffer is still in use");

        drop(still_held);
    }
}
