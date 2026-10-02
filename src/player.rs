//! Player — simple wrapper for single-source playback to a single output.

use std::path::Path;

use crate::compositor::Compositor;
use crate::error::Result;
use crate::geometry::Rect;
use crate::output::Output;
use crate::output_layer::OutputLayer;

/// A simple video player: one source playing to one output.
///
/// Player wraps the Compositor for the common case of playing a single video file
/// to a single output (monitor or texture). For more complex multi-source or
/// multi-output scenarios, use Compositor directly.
///
/// # Rendering
///
/// Player renders to a wgpu texture, which the host application can composite
/// into its own scene. The close button is rendered as a UI overlay stub.
pub struct Player {
    compositor: Compositor,
    video_source_id: String,
    output_index: usize,
}

impl Player {
    /// Create a player for a single video file.
    ///
    /// The output defaults to the full monitor/screen size.
    pub fn from_file<P: AsRef<Path>>(path: P, width: u32, height: u32) -> Result<Self> {
        let mut compositor = Compositor::new();

        // Add the video source
        let video_source_id = compositor.add_source_from_file(path)?;

        // Create a full-screen output with the source as a single layer
        let mut output = Output::new(width, height);
        let layer = OutputLayer::new(
            &video_source_id,
            Rect::new(0.0, 0.0, width as f64, height as f64),
        );
        output.add_layer(layer);

        let output_index = 0;
        compositor.add_output(output);

        Ok(Player {
            compositor,
            video_source_id,
            output_index,
        })
    }

    /// Advance playback by the given delta time.
    pub fn advance(&mut self, delta: std::time::Duration) -> Result<()> {
        self.compositor.advance(delta)
    }

    /// Render the current frame to an output texture.
    ///
    /// In Phase 1, this is stubbed. Full implementation would composite layers
    /// to the output texture and render the close button UI.
    pub fn render(&mut self) -> Result<()> {
        self.compositor.render_output(self.output_index)
    }

    /// Get mutable access to the underlying compositor for advanced control.
    pub fn compositor_mut(&mut self) -> &mut Compositor {
        &mut self.compositor
    }

    /// Get immutable access to the underlying compositor.
    pub fn compositor(&self) -> &Compositor {
        &self.compositor
    }

    /// Get the video source ID (for reference).
    pub fn video_source_id(&self) -> &str {
        &self.video_source_id
    }

    /// Get the current output (for inspection or modification).
    pub fn output(&self) -> Option<&Output> {
        self.compositor.get_output(self.output_index)
    }

    /// Get the current output (mutable).
    pub fn output_mut(&mut self) -> Option<&mut Output> {
        self.compositor.get_output_mut(self.output_index)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn player_creation_structure_test() {
        // In real tests, we'd create a player from a test video file
        // For Phase 1, this validates the structure compiles
    }
}
