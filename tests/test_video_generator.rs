//! Test video generator — creates synthetic test videos for validation.
//!
//! This module provides utilities to generate test video frames and files
//! for validating the demux → decode → render pipeline without requiring
//! real video files committed to the repository.

#[cfg(feature = "demux")]
mod generator {
    use vtome::{Frame, PixelFormat, ColorSpace};
    use std::time::Duration;

    /// Generate a test checkerboard frame.
    pub fn checkerboard(width: u32, height: u32, frame_num: u32) -> Frame {
        let block_size = 32;
        let mut data = vec![0u8; (width * height * 4) as usize];

        for y in 0..height {
            for x in 0..width {
                let block_x = x / block_size;
                let block_y = y / block_size;
                let is_white = (block_x + block_y) % 2 == 0;

                let color = if is_white { 255u8 } else { 0u8 };

                let idx = ((y * width + x) * 4) as usize;
                data[idx] = color;
                data[idx + 1] = color;
                data[idx + 2] = color;
                data[idx + 3] = 255;
            }
        }

        Frame::packed(
            width,
            height,
            PixelFormat::Rgba8,
            ColorSpace::srgb(),
            Duration::from_millis(frame_num as u64 * 41),
            data,
        )
        .expect("valid checkerboard frame")
    }

    /// Generate a gradient frame (useful for testing color spaces).
    pub fn gradient(width: u32, height: u32, frame_num: u32) -> Frame {
        let mut data = vec![0u8; (width * height * 4) as usize];

        for y in 0..height {
            for x in 0..width {
                let r = ((x as f32 / width as f32) * 255.0) as u8;
                let g = ((y as f32 / height as f32) * 255.0) as u8;
                let b = 128u8;

                let idx = ((y * width + x) * 4) as usize;
                data[idx] = r;
                data[idx + 1] = g;
                data[idx + 2] = b;
                data[idx + 3] = 255;
            }
        }

        Frame::packed(
            width,
            height,
            PixelFormat::Rgba8,
            ColorSpace::srgb(),
            Duration::from_millis(frame_num as u64 * 41),
            data,
        )
        .expect("valid gradient frame")
    }

    /// Generate a color bars frame (standard test pattern).
    pub fn color_bars(width: u32, height: u32, frame_num: u32) -> Frame {
        let mut data = vec![0u8; (width * height * 4) as usize];

        let colors = [
            (255u8, 255u8, 255u8),  // White
            (255, 255, 0),          // Yellow
            (0, 255, 255),          // Cyan
            (0, 255, 0),            // Green
            (255, 0, 255),          // Magenta
            (255, 0, 0),            // Red
            (0, 0, 255),            // Blue
            (0, 0, 0),              // Black
        ];

        let bar_width = width / colors.len() as u32;

        for y in 0..height {
            for x in 0..width {
                let bar_index = (x / bar_width).min(colors.len() as u32 - 1) as usize;
                let (r, g, b) = colors[bar_index];

                let idx = ((y * width + x) * 4) as usize;
                data[idx] = r;
                data[idx + 1] = g;
                data[idx + 2] = b;
                data[idx + 3] = 255;
            }
        }

        Frame::packed(
            width,
            height,
            PixelFormat::Rgba8,
            ColorSpace::srgb(),
            Duration::from_millis(frame_num as u64 * 41),
            data,
        )
        .expect("valid color bars frame")
    }

    /// Generate a moving test pattern (useful for motion testing).
    pub fn moving_circle(width: u32, height: u32, frame_num: u32) -> Frame {
        let mut data = vec![0u8; (width * height * 4) as usize];

        let circle_radius = 50.0;
        let center_x = ((frame_num as f32 * 5.0) % width as f32) as f32;
        let center_y = height as f32 / 2.0;

        for y in 0..height {
            for x in 0..width {
                let dx = x as f32 - center_x;
                let dy = y as f32 - center_y;
                let distance = (dx * dx + dy * dy).sqrt();

                let (r, g, b) = if distance < circle_radius {
                    (255u8, 0, 0)  // Red circle
                } else {
                    (0, 0, 0)      // Black background
                };

                let idx = ((y * width + x) * 4) as usize;
                data[idx] = r;
                data[idx + 1] = g;
                data[idx + 2] = b;
                data[idx + 3] = 255;
            }
        }

        Frame::packed(
            width,
            height,
            PixelFormat::Rgba8,
            ColorSpace::srgb(),
            Duration::from_millis(frame_num as u64 * 41),
            data,
        )
        .expect("valid moving circle frame")
    }

    /// Generate a ramp frame (tests gradation smoothness).
    pub fn grayscale_ramp(width: u32, height: u32, frame_num: u32) -> Frame {
        let mut data = vec![0u8; (width * height * 4) as usize];

        for y in 0..height {
            for x in 0..width {
                let gray = ((x as f32 / width as f32) * 255.0) as u8;

                let idx = ((y * width + x) * 4) as usize;
                data[idx] = gray;
                data[idx + 1] = gray;
                data[idx + 2] = gray;
                data[idx + 3] = 255;
            }
        }

        Frame::packed(
            width,
            height,
            PixelFormat::Rgba8,
            ColorSpace::srgb(),
            Duration::from_millis(frame_num as u64 * 41),
            data,
        )
        .expect("valid grayscale ramp frame")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn test_checkerboard_generation() {
            let frame = checkerboard(640, 480, 0);
            assert_eq!(frame.width(), 640);
            assert_eq!(frame.height(), 480);
            assert_eq!(frame.format(), PixelFormat::Rgba8);
        }

        #[test]
        fn test_gradient_generation() {
            let frame = gradient(640, 480, 0);
            assert_eq!(frame.width(), 640);
            assert_eq!(frame.height(), 480);
            assert!(frame.pts() >= Duration::ZERO);
        }

        #[test]
        fn test_color_bars_generation() {
            let frame = color_bars(640, 480, 0);
            assert_eq!(frame.width(), 640);
            assert_eq!(frame.height(), 480);
        }

        #[test]
        fn test_moving_circle_generation() {
            let frames: Vec<_> = (0..5)
                .map(|i| moving_circle(640, 480, i))
                .collect();

            assert_eq!(frames.len(), 5);
            for frame in frames {
                assert_eq!(frame.width(), 640);
                assert_eq!(frame.height(), 480);
            }
        }

        #[test]
        fn test_grayscale_ramp_generation() {
            let frame = grayscale_ramp(640, 480, 0);
            assert_eq!(frame.width(), 640);
            assert_eq!(frame.height(), 480);
        }
    }
}

#[cfg(not(feature = "demux"))]
fn main() {
    eprintln!("Test video generator requires --features demux");
}
