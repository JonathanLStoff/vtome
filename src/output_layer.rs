//! Output layers — video sources positioned and sized within an output.

use crate::geometry::{Fit, Rect};

/// A single layer in an output, referencing a source with position and styling.
///
/// Stacked by z-index, 0 on top: an output is drawn from its highest z-index
/// down to 0, and layers with the same z-index stack in the order they were
/// added, the later one on top. Each layer independently specifies where and
/// how large its source appears.
///
/// # Coordinates
///
/// Position and size are in output space, where (0, 0) is the top-left corner
/// of the output texture, regardless of where the output is placed on a monitor.
#[derive(Clone, Debug)]
pub struct OutputLayer {
    /// Persistent ID of the video source to render (references VideoSource).
    pub video_source_id: String,

    /// Position and size of this layer within the output, in output coordinates.
    pub bounds: Rect,

    /// How the source's picture sits inside `bounds` when the shapes differ.
    /// [`Fit::Contain`] by default: the whole picture, letterboxed.
    pub fit: Fit,

    /// Stacking order: 0 is topmost, higher values are further back.
    pub z_index: i32,

    /// Layer opacity: 0.0 (transparent) to 1.0 (opaque).
    pub opacity: f32,

    /// Whether this layer is visible. Hidden layers are skipped during rendering.
    pub visible: bool,
}

impl OutputLayer {
    /// Create a new layer referencing a video source.
    pub fn new(video_source_id: impl Into<String>, bounds: Rect) -> Self {
        OutputLayer {
            video_source_id: video_source_id.into(),
            bounds,
            fit: Fit::Contain,
            z_index: 0,
            opacity: 1.0,
            visible: true,
        }
    }

    /// Set how the picture sits inside the bounds.
    pub fn with_fit(mut self, fit: Fit) -> Self {
        self.fit = fit;
        self
    }

    /// Set the stacking order (z-index).
    pub fn with_z_index(mut self, z_index: i32) -> Self {
        self.z_index = z_index;
        self
    }

    /// Set the opacity (0.0 to 1.0).
    pub fn with_opacity(mut self, opacity: f32) -> Self {
        self.opacity = opacity.clamp(0.0, 1.0);
        self
    }

    /// Set visibility.
    pub fn with_visible(mut self, visible: bool) -> Self {
        self.visible = visible;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_creation_and_builder_pattern() {
        let rect = Rect::new(100.0, 50.0, 400.0, 300.0);
        let layer = OutputLayer::new("video1", rect)
            .with_z_index(2)
            .with_opacity(0.8)
            .with_visible(true);

        assert_eq!(layer.video_source_id, "video1");
        assert_eq!(layer.z_index, 2);
        assert_eq!(layer.opacity, 0.8);
        assert!(layer.visible);
    }

    #[test]
    fn opacity_clamps_to_valid_range() {
        let rect = Rect::new(0.0, 0.0, 100.0, 100.0);
        let layer = OutputLayer::new("src", rect).with_opacity(1.5);
        assert_eq!(layer.opacity, 1.0);

        let layer = OutputLayer::new("src", rect).with_opacity(-0.5);
        assert_eq!(layer.opacity, 0.0);
    }
}
