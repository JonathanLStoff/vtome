//! Video frame plugins for effects and transformations.

use crate::frame::Frame;
use crate::error::Result;

/// A video frame plugin that processes frames in-order.
///
/// Plugins modify video feeds at the frame level, enabling effects,
/// transformations, and custom rendering.
///
/// # Examples
///
/// A rotation plugin might read the frame, apply geometric transformation,
/// and return the result.
pub trait Plugin: Send + Sync {
    /// Process a frame and return the result.
    ///
    /// Takes ownership of the input frame. Implementations may reuse the
    /// buffer or allocate a new one depending on the transformation.
    fn process(&mut self, frame: Frame) -> Result<Frame>;

    /// Reset internal state (e.g., after seeking or pause).
    fn reset(&mut self) -> Result<()> {
        Ok(())
    }

    /// Human-readable name for this plugin.
    fn name(&self) -> &str;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct PassthroughPlugin;

    impl Plugin for PassthroughPlugin {
        fn process(&mut self, frame: Frame) -> Result<Frame> {
            Ok(frame)
        }

        fn name(&self) -> &str {
            "passthrough"
        }
    }

    #[test]
    fn plugin_trait_is_object_safe() {
        let plugin: Box<dyn Plugin> = Box::new(PassthroughPlugin);
        assert_eq!(plugin.name(), "passthrough");
    }
}
