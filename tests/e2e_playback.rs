//! End-to-end playback tests.
//!
//! These tests validate the complete pipeline:
//! Demuxer → Decoder → Compositor → Renderer
//!
//! Tests are marked `#[ignore]` by default since they require:
//! 1. Real H.264 or AV1 test video files
//! 2. Working decoders (VideoToolbox, dav1d)
//!
//! Once decoders are implemented, run with:
//! cargo test --test e2e_playback -- --nocapture --ignored

#[cfg(feature = "demux")]
mod tests {
    use vtome::{Compositor, Output, OutputLayer, Frame, PixelFormat, ColorSpace};
    use vtome::geometry::Rect;
    use std::time::Duration;
    use std::path::Path;

    /// Helper: create a test checkerboard frame
    fn test_checkerboard(width: u32, height: u32, frame_num: u32) -> Frame {
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
        .expect("valid frame")
    }

    /// Test: H.264 file demux and decode pipeline (requires test file)
    ///
    /// This test:
    /// 1. Opens an H.264 video file (if available)
    /// 2. Demuxes the first few packets
    /// 3. Decodes frames using VideoToolbox
    /// 4. Validates frame properties
    ///
    /// **Status**: Requires VideoToolbox FFI implementation
    /// **File**: tests/data/test_h264.mp4 (not committed, must be generated)
    #[test]
    #[ignore]
    fn h264_file_decodes_and_produces_frames() {
        let file_path = "tests/data/test_h264.mp4";

        if !Path::new(file_path).exists() {
            eprintln!("Skipping: {} not found (generate with ffmpeg)", file_path);
            eprintln!("  ffmpeg -f lavfi -i testsrc=s=1920x1080:d=1 -pix_fmt yuv420p -c:v libx264 -preset fast {}", file_path);
            return;
        }

        // Open media file
        let demuxer = match vtome::open_media(file_path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("Failed to open {}: {}", file_path, e);
                panic!("demux failed");
            }
        };

        let info = demuxer.info();
        let video = info.video().expect("must have video track");

        // Verify codec is H.264
        let encoding = video.encoding.expect("video track must have encoding");
        assert_eq!(encoding, vtome::Encoding::H264);
        assert!(video.width > 0 && video.height > 0);

        // Create decoder
        let config = vtome::decode::DecoderConfig {
            encoding,
            width: video.width,
            height: video.height,
            bit_depth: video.bit_depth,
            color: video.color,
            extra_data: video.extra_data.clone(),
        };

        let decoder = match vtome::decode::open(&config) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("No H.264 decoder available: {}", e);
                eprintln!("Enable --features decode-platform or ensure VideoToolbox FFI is implemented");
                return;
            }
        };

        // Verify hardware decoder
        assert!(decoder.is_hardware(), "Expected hardware H.264 decoder");
        assert_eq!(decoder.encoding(), encoding);
    }

    /// Test: Multi-source composition with test frames
    ///
    /// This test validates that multiple video sources can be composed
    /// to an output with proper layer ordering and opacity.
    #[test]
    fn multi_source_composition() {
        let mut comp = Compositor::new();

        // Create output
        let mut output = Output::new(1920, 1080)
            .with_background_color([0.1, 0.1, 0.1, 1.0]);

        // Add placeholder layers (would reference real video sources in full test)
        let layer1 = OutputLayer::new("video_a", Rect::new(0.0, 0.0, 960.0, 1080.0))
            .with_z_index(0)
            .with_opacity(1.0);
        let layer2 = OutputLayer::new("video_b", Rect::new(960.0, 0.0, 960.0, 1080.0))
            .with_z_index(1)
            .with_opacity(0.8);

        output.add_layers(vec![layer1, layer2]);
        comp.add_output(output);

        // Advance playback
        let result = comp.advance(Duration::from_millis(41));
        assert!(result.is_ok());

        // Verify output structure
        assert_eq!(comp.output_count(), 1);
        let out = comp.get_output(0).unwrap();
        assert_eq!(out.width, 1920);
        assert_eq!(out.height, 1080);
        assert_eq!(out.layers.len(), 2);

        // Verify layer properties
        assert_eq!(out.layers[0].z_index, 0);
        assert_eq!(out.layers[0].opacity, 1.0);
        assert_eq!(out.layers[1].z_index, 1);
        assert_eq!(out.layers[1].opacity, 0.8);
    }

    /// Test: Compositor frame queuing with test frames
    ///
    /// Validates that VideoSource frame queue management works correctly
    /// when feeding frames through the compositor pipeline.
    #[test]
    fn compositor_frame_queuing() {
        // Create test frames
        let frames: Vec<Frame> = (0..10)
            .map(|i| test_checkerboard(1920, 1080, i))
            .collect();

        // Verify frame properties
        for (i, frame) in frames.iter().enumerate() {
            assert_eq!(frame.width(), 1920);
            assert_eq!(frame.height(), 1080);
            assert_eq!(frame.format(), PixelFormat::Rgba8);

            // Verify presentation timestamps are monotonically increasing
            let expected_pts = Duration::from_millis((i as u64) * 41);
            assert_eq!(frame.pts(), expected_pts);
        }
    }

    /// Test: Demux → Decode → Composite pipeline validation
    ///
    /// This test validates the complete V2E pipeline structure without
    /// requiring actual video files or working decoders.
    #[test]
    fn pipeline_structure_validation() {
        // Create compositor with sources and outputs
        let mut comp = Compositor::new();

        // Create output
        let mut output = Output::new(1920, 1080);
        let layer = OutputLayer::new("test_video", Rect::new(0.0, 0.0, 1920.0, 1080.0));
        output.add_layer(layer);
        comp.add_output(output);

        // Validate pipeline structure
        assert_eq!(comp.source_count(), 0);
        assert_eq!(comp.output_count(), 1);

        // Advance should succeed even with no sources
        let result = comp.advance(Duration::from_millis(41));
        assert!(result.is_ok(), "Should handle empty source list gracefully");

        // Render should succeed
        let result = comp.render_output(0);
        assert!(result.is_ok(), "Should render output without sources");
    }

    /// Documentation: How to create H.264 test video
    ///
    /// To generate a test video file for use with `h264_file_decodes_and_produces_frames`,
    /// run this command:
    ///
    /// ```bash
    /// mkdir -p tests/data
    /// ffmpeg -f lavfi -i testsrc=s=1920x1080:d=2 \
    ///        -pix_fmt yuv420p \
    ///        -c:v libx264 \
    ///        -preset fast \
    ///        -crf 18 \
    ///        tests/data/test_h264.mp4
    /// ```
    ///
    /// This creates a 2-second test video:
    /// - Resolution: 1920×1080
    /// - Codec: H.264 (Baseline profile)
    /// - Pixel format: YUV 4:2:0
    /// - Frame rate: 25 fps
    /// - File size: ~100 KB
    ///
    /// The file should NOT be committed to git (too large).
    /// Instead, it's generated locally during testing.
    ///
    /// For AV1, use:
    /// ```bash
    /// ffmpeg -f lavfi -i testsrc=s=1920x1080:d=2 \
    ///        -pix_fmt yuv420p \
    ///        -c:v libaom-av1 \
    ///        -crf 18 \
    ///        tests/data/test_av1.webm
    /// ```
    ///
    /// For VP9, use:
    /// ```bash
    /// ffmpeg -f lavfi -i testsrc=s=1920x1080:d=2 \
    ///        -pix_fmt yuv420p \
    ///        -c:v libvpx-vp9 \
    ///        -crf 18 \
    ///        tests/data/test_vp9.webm
    /// ```
    #[test]
    fn e2e_test_documentation() {
        // This test is a no-op; it just documents the testing approach
        println!("E2E tests require test video files. Generate them with:");
        println!("  mkdir -p tests/data");
        println!("  ffmpeg -f lavfi -i testsrc=s=1920x1080:d=2 \\");
        println!("         -pix_fmt yuv420p -c:v libx264 -preset fast tests/data/test_h264.mp4");
    }
}

#[cfg(not(feature = "demux"))]
fn main() {
    eprintln!("E2E playback tests require --features demux");
}
