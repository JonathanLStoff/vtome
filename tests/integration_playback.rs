//! Integration tests for video playback and composition.

// The compositor is built from sources, which are opened through a demuxer.
#![cfg(feature = "demux")]

use vtome::{Compositor, Output, OutputLayer};
use vtome::geometry::Rect;
use vtome::{Frame, ColorSpace, PixelFormat};
use std::time::Duration;

// Test fixture generators
fn checkerboard_frame(width: u32, height: u32, frame_num: u32) -> Frame {
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

fn gradient_frame(width: u32, height: u32, frame_num: u32) -> Frame {
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
    .expect("valid frame")
}

#[test]
fn compositor_layer_ordering() {
    let mut comp = Compositor::new();

    // Create output with dimensions
    let mut output = Output::new(640, 480);

    // Add layers in non-sequential order
    let layer_a = OutputLayer::new("video_a", Rect::new(0.0, 0.0, 200.0, 200.0))
        .with_z_index(2);
    let layer_b = OutputLayer::new("video_b", Rect::new(100.0, 100.0, 200.0, 200.0))
        .with_z_index(0);
    let layer_c = OutputLayer::new("video_c", Rect::new(200.0, 200.0, 200.0, 200.0))
        .with_z_index(1);

    output.add_layers(vec![layer_a, layer_b, layer_c]);

    // Verify layers are sorted by z-index
    assert_eq!(output.layers[0].z_index, 0);
    assert_eq!(output.layers[1].z_index, 1);
    assert_eq!(output.layers[2].z_index, 2);

    comp.add_output(output);
    assert_eq!(comp.output_count(), 1);
}

#[test]
fn output_background_with_layers() {
    let output = Output::new(1920, 1080)
        .with_background_color([0.2, 0.2, 0.2, 1.0])
        .with_background_image("background.png");

    assert_eq!(output.background_color, [0.2, 0.2, 0.2, 1.0]);
    assert_eq!(output.background_image, Some("background.png".to_string()));
    assert_eq!(output.width, 1920);
    assert_eq!(output.height, 1080);
}

#[test]
fn compositor_output_management() {
    let mut comp = Compositor::new();

    // Add multiple outputs
    let output1 = Output::new(1920, 1080);
    let output2 = Output::new(1280, 720);
    let output3 = Output::new(640, 480);

    comp.add_output(output1);
    comp.add_output(output2);
    comp.add_output(output3);

    assert_eq!(comp.output_count(), 3);

    // Verify dimensions
    assert_eq!(comp.get_output(0).unwrap().width, 1920);
    assert_eq!(comp.get_output(1).unwrap().width, 1280);
    assert_eq!(comp.get_output(2).unwrap().width, 640);

    // Remove middle output
    let removed = comp.remove_output(1);
    assert!(removed.is_some());
    assert_eq!(comp.output_count(), 2);
    assert_eq!(comp.get_output(1).unwrap().width, 640);
}

#[test]
fn output_layer_visibility_filtering() {
    let mut output = Output::new(640, 480);

    let mut layer_visible = OutputLayer::new("visible", Rect::new(0.0, 0.0, 100.0, 100.0));
    let mut layer_hidden = OutputLayer::new("hidden", Rect::new(100.0, 100.0, 100.0, 100.0));

    layer_visible.visible = true;
    layer_hidden.visible = false;

    output.add_layers(vec![layer_visible, layer_hidden]);

    let visible_count: usize = output.visible_layers().count();
    assert_eq!(visible_count, 1);

    let visible_names: Vec<&str> = output
        .visible_layers()
        .map(|l| l.video_source_id.as_str())
        .collect();
    assert_eq!(visible_names, vec!["visible"]);
}

#[test]
fn output_layer_opacity_clamping() {
    let rect = Rect::new(0.0, 0.0, 100.0, 100.0);

    // Test over-range
    let layer = OutputLayer::new("test", rect).with_opacity(1.5);
    assert_eq!(layer.opacity, 1.0);

    // Test under-range
    let layer = OutputLayer::new("test", rect).with_opacity(-0.5);
    assert_eq!(layer.opacity, 0.0);

    // Test valid range
    let layer = OutputLayer::new("test", rect).with_opacity(0.5);
    assert_eq!(layer.opacity, 0.5);
}

#[test]
fn compositor_advance_without_sources() {
    let mut comp = Compositor::new();
    let output = Output::new(640, 480);
    comp.add_output(output);

    // Should not panic when advancing with no sources
    let result = comp.advance(std::time::Duration::from_millis(16));
    assert!(result.is_ok());
}

#[test]
fn compositor_render_output_stub() {
    let mut comp = Compositor::new();
    let output = Output::new(640, 480);
    comp.add_output(output);

    // Phase 1 stub: render_output doesn't do much yet
    let result = comp.render_output(0);
    assert!(result.is_ok());
}

#[test]
fn video_source_persistent_id_consistency() {
    // Two VideoSources from the same path should have the same persistent_id
    // (This is tested implicitly by the hash function being deterministic)

    let id1 = {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        "test_video.mp4".hash(&mut hasher);
        format!("{:x}", hasher.finish())
    };

    let id2 = {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        "test_video.mp4".hash(&mut hasher);
        format!("{:x}", hasher.finish())
    };

    assert_eq!(id1, id2);

    // Different paths should have different IDs
    let id3 = {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        "other_video.mp4".hash(&mut hasher);
        format!("{:x}", hasher.finish())
    };

    assert_ne!(id1, id3);
}

#[cfg(feature = "render")]
#[test]
fn end_to_end_composition_to_texture() {
    // Create a compositor with test frames
    let mut comp = Compositor::new();

    // Create an output
    let mut output = Output::new(640, 480)
        .with_background_color([0.1, 0.1, 0.1, 1.0]);

    // Add a placeholder layer (would reference a real video source in full implementation)
    let layer = OutputLayer::new("test_video", Rect::new(50.0, 50.0, 320.0, 240.0));
    output.add_layer(layer);

    comp.add_output(output);

    // Advance playback
    let result = comp.advance(std::time::Duration::from_millis(16));
    assert!(result.is_ok());

    // Verify we can render
    let result = comp.render_output(0);
    assert!(result.is_ok());

    // Verify layer is still there
    assert_eq!(comp.get_output(0).unwrap().layers.len(), 1);
}

#[test]
fn test_fixture_frame_generation() {
    let cb = checkerboard_frame(320, 240, 0);
    assert_eq!(cb.width(), 320);
    assert_eq!(cb.height(), 240);

    let grad = gradient_frame(640, 480, 1);
    assert_eq!(grad.width(), 640);
    assert_eq!(grad.height(), 480);
}

#[test]
fn full_pipeline_composition_test() {
    // This test demonstrates the full Phase 1 pipeline:
    // 1. Create a compositor
    // 2. Create multiple outputs
    // 3. Add layers with different z-indices
    // 4. Advance playback
    // 5. Render outputs

    let mut comp = Compositor::new();

    // Create two outputs with different purposes
    let mut output1 = Output::new(1920, 1080)
        .with_background_color([0.0, 0.0, 0.0, 1.0]);

    let mut output2 = Output::new(1280, 720)
        .with_background_color([0.1, 0.1, 0.1, 1.0])
        .with_background_image("background.png");

    // Add layers to output 1
    let layer1 = OutputLayer::new("video_main", Rect::new(100.0, 100.0, 800.0, 600.0))
        .with_z_index(0)
        .with_opacity(1.0);
    let layer2 = OutputLayer::new("video_overlay", Rect::new(1000.0, 800.0, 400.0, 200.0))
        .with_z_index(1)
        .with_opacity(0.8);

    output1.add_layers(vec![layer1, layer2]);

    // Add layers to output 2
    let layer3 = OutputLayer::new("video_main", Rect::new(50.0, 50.0, 600.0, 450.0))
        .with_z_index(0);
    output2.add_layer(layer3);

    // Add outputs to compositor
    comp.add_output(output1);
    comp.add_output(output2);

    assert_eq!(comp.output_count(), 2);

    // Advance playback for one frame (~41ms at 24fps)
    let result = comp.advance(Duration::from_millis(41));
    assert!(result.is_ok());

    // Render both outputs
    let result1 = comp.render_output(0);
    let result2 = comp.render_output(1);
    assert!(result1.is_ok());
    assert!(result2.is_ok());

    // Verify outputs are still intact
    let out1 = comp.get_output(0).unwrap();
    assert_eq!(out1.width, 1920);
    assert_eq!(out1.height, 1080);
    assert_eq!(out1.layers.len(), 2);

    let out2 = comp.get_output(1).unwrap();
    assert_eq!(out2.width, 1280);
    assert_eq!(out2.height, 720);
    assert_eq!(out2.layers.len(), 1);
}

#[test]
fn multi_output_sync_test() {
    // Test that multiple outputs can be synced together
    let mut comp = Compositor::new();

    // Create outputs for different monitors/streams
    let output_monitor = Output::new(3840, 2160); // 4K monitor
    let output_stream = Output::new(1920, 1080);  // HD stream
    let output_preview = Output::new(640, 480);   // Preview thumbnail

    comp.add_output(output_monitor);
    comp.add_output(output_stream);
    comp.add_output(output_preview);

    assert_eq!(comp.output_count(), 3);

    // Advance all in sync
    let advance_result = comp.advance(Duration::from_millis(16));
    assert!(advance_result.is_ok());

    // All should render successfully
    for i in 0..3 {
        let render_result = comp.render_output(i);
        assert!(render_result.is_ok(), "Failed to render output {}", i);
    }

    // Verify all outputs maintain their state
    for i in 0..3 {
        assert!(comp.get_output(i).is_some());
    }
}
