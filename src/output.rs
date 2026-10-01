//! Output scenes — composition targets with multiple layers and backgrounds.

use crate::geometry::Fit;
use crate::output_layer::OutputLayer;

/// An output or scene: a render target with multiple layers, background, and dimensions.
///
/// An Output is independent of any particular monitor or device. It specifies:
/// - Dimensions (width, height)
/// - Background (solid color or image path)
/// - Layers of video sources, each with position, size, and z-order
///
/// Multiple Outputs can render in parallel to different monitors or streams.
/// When embedded in a host application, the output texture is composited by the host.
#[derive(Clone, Debug)]
pub struct Output {
    /// Output width in pixels.
    pub width: u32,

    /// Output height in pixels.
    pub height: u32,

    /// Background color as RGBA: [red, green, blue, alpha], each 0.0-1.0.
    ///
    /// Default is [0.0, 0.0, 0.0, 1.0] (opaque black).
    pub background_color: [f32; 4],

    /// Optional background image path. If present, this image is rendered
    /// after the background color and before the layers.
    ///
    /// Path is relative to the application's working directory or absolute.
    pub background_image: Option<String>,

    /// How the background image fills the output. [`Fit::Cover`] by default,
    /// the way a desktop wallpaper does: no bars, cropped rather than
    /// stretched.
    pub background_fit: Fit,

    /// Layers to render on top of the background, ordered by z-index.
    pub layers: Vec<OutputLayer>,
}

impl Output {
    /// Create a new output with the given dimensions and a default black background.
    pub fn new(width: u32, height: u32) -> Self {
        Output {
            width,
            height,
            background_color: [0.0, 0.0, 0.0, 1.0],
            background_image: None,
            background_fit: Fit::Cover,
            layers: Vec::new(),
        }
    }

    /// Set the background color as RGBA (each component 0.0-1.0).
    pub fn with_background_color(mut self, color: [f32; 4]) -> Self {
        self.background_color = [
            color[0].clamp(0.0, 1.0),
            color[1].clamp(0.0, 1.0),
            color[2].clamp(0.0, 1.0),
            color[3].clamp(0.0, 1.0),
        ];
        self
    }

    /// Set the background image path. If set, the image is rendered after color.
    pub fn with_background_image(mut self, path: impl Into<String>) -> Self {
        self.background_image = Some(path.into());
        self
    }

    /// Set how the background image fills the output.
    pub fn with_background_fit(mut self, fit: Fit) -> Self {
        self.background_fit = fit;
        self
    }

    /// Add a layer to this output.
    pub fn add_layer(&mut self, layer: OutputLayer) {
        self.layers.push(layer);
        self.sort_layers();
    }

    /// Add multiple layers at once.
    pub fn add_layers(&mut self, layers: impl IntoIterator<Item = OutputLayer>) {
        self.layers.extend(layers);
        self.sort_layers();
    }

    /// Remove a layer by video source ID. Returns true if a layer was removed.
    pub fn remove_layer(&mut self, video_source_id: &str) -> bool {
        let before_len = self.layers.len();
        self.layers.retain(|layer| layer.video_source_id != video_source_id);
        self.layers.len() < before_len
    }

    /// Get a mutable reference to a layer by source ID.
    pub fn get_layer_mut(&mut self, video_source_id: &str) -> Option<&mut OutputLayer> {
        self.layers.iter_mut().find(|layer| layer.video_source_id == video_source_id)
    }

    /// Keep layers ordered by z-index, 0 — the top — first. Stable, so layers
    /// with the same z-index stay in the order they were added.
    fn sort_layers(&mut self) {
        self.layers.sort_by_key(|layer| layer.z_index);
    }

    /// Get visible layers, top first (lowest z-index first). Drawing goes the
    /// other way — see [`OutputLayer`].
    pub fn visible_layers(&self) -> impl Iterator<Item = &OutputLayer> {
        self.layers.iter().filter(|layer| layer.visible)
    }

    /// Get a layer by video source ID.
    pub fn get_layer(&self, video_source_id: &str) -> Option<&OutputLayer> {
        self.layers.iter().find(|layer| layer.video_source_id == video_source_id)
    }

    /// Check if a layer exists by source ID.
    pub fn has_layer(&self, video_source_id: &str) -> bool {
        self.layers.iter().any(|layer| layer.video_source_id == video_source_id)
    }

    /// Get the number of visible layers.
    pub fn visible_layer_count(&self) -> usize {
        self.layers.iter().filter(|layer| layer.visible).count()
    }

    /// Get the number of total layers.
    pub fn layer_count(&self) -> usize {
        self.layers.len()
    }

    /// Clear all layers from this output.
    pub fn clear_layers(&mut self) {
        self.layers.clear();
    }

    /// Set the background color as RGB (alpha defaults to 1.0).
    pub fn with_background_rgb(mut self, r: f32, g: f32, b: f32) -> Self {
        self.background_color = [
            r.clamp(0.0, 1.0),
            g.clamp(0.0, 1.0),
            b.clamp(0.0, 1.0),
            1.0,
        ];
        self
    }

    /// Set the background to transparent (alpha = 0.0).
    pub fn with_transparent_background(mut self) -> Self {
        self.background_color = [0.0, 0.0, 0.0, 0.0];
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Rect;

    #[test]
    fn output_creation() {
        let output = Output::new(1920, 1080);
        assert_eq!(output.width, 1920);
        assert_eq!(output.height, 1080);
        assert_eq!(output.background_color, [0.0, 0.0, 0.0, 1.0]);
        assert!(output.background_image.is_none());
        assert!(output.layers.is_empty());
    }

    #[test]
    fn output_with_custom_background() {
        let output = Output::new(1280, 720)
            .with_background_color([0.2, 0.4, 0.6, 1.0])
            .with_background_image("background.png");

        assert_eq!(output.background_color, [0.2, 0.4, 0.6, 1.0]);
        assert_eq!(output.background_image, Some("background.png".to_string()));
    }

    #[test]
    fn layers_are_sorted_by_z_index() {
        let mut output = Output::new(640, 480);

        let layer_a = OutputLayer::new("video_a", Rect::new(0.0, 0.0, 200.0, 200.0))
            .with_z_index(2);
        let layer_b = OutputLayer::new("video_b", Rect::new(100.0, 100.0, 200.0, 200.0))
            .with_z_index(0);
        let layer_c = OutputLayer::new("video_c", Rect::new(200.0, 200.0, 200.0, 200.0))
            .with_z_index(1);

        output.add_layers(vec![layer_a, layer_b, layer_c]);

        // After sorting, should be: b(0), c(1), a(2)
        assert_eq!(output.layers[0].z_index, 0);
        assert_eq!(output.layers[1].z_index, 1);
        assert_eq!(output.layers[2].z_index, 2);
    }

    #[test]
    fn remove_layer_by_source_id() {
        let mut output = Output::new(640, 480);
        output.add_layer(OutputLayer::new("video_a", Rect::new(0.0, 0.0, 100.0, 100.0)));
        output.add_layer(OutputLayer::new("video_b", Rect::new(100.0, 100.0, 100.0, 100.0)));

        assert_eq!(output.layers.len(), 2);
        assert!(output.remove_layer("video_a"));
        assert_eq!(output.layers.len(), 1);
        assert_eq!(output.layers[0].video_source_id, "video_b");
    }

    #[test]
    fn visible_layers_filter() {
        let mut output = Output::new(640, 480);
        let mut layer_a = OutputLayer::new("video_a", Rect::new(0.0, 0.0, 100.0, 100.0));
        let mut layer_b = OutputLayer::new("video_b", Rect::new(100.0, 100.0, 100.0, 100.0));

        layer_a.visible = true;
        layer_b.visible = false;

        output.add_layers(vec![layer_a, layer_b]);

        let visible: Vec<_> = output.visible_layers().collect();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].video_source_id, "video_a");
    }
}
