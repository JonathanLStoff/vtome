//! VP9 decoder using libvpx.
//!
//! Royalty-free VP9 decoding as a fallback where AV1 hardware support is not available.

use crate::error::{Error, Result};
use crate::frame::Frame;
use crate::identify::Encoding;
use crate::media::Packet;

/// VP9 decoder (software, royalty-free).
///
/// Supports VP9 decoding via libvpx C bindings.
/// VP9 is the compatibility fallback when AV1 hardware decode is not available.
///
/// # Implementation Status
///
/// Stub implementation. Requires:
/// 1. vpx-sys crate for libvpx FFI
/// 2. Decoder context creation and initialization
/// 3. Packet feeding to decoder
/// 4. Frame extraction and YUV handling
pub struct Vp9Decoder {
    _marker: std::marker::PhantomData<()>,
}

impl Vp9Decoder {
    /// Create a new VP9 decoder.
    pub fn new() -> Result<Self> {
        // Full implementation would initialize libvpx decoder here
        Ok(Vp9Decoder {
            _marker: std::marker::PhantomData,
        })
    }
}

impl crate::decode::Decoder for Vp9Decoder {
    fn encoding(&self) -> Encoding {
        Encoding::Vp9
    }

    fn decode(&mut self, _packet: &Packet) -> Result<Option<Frame>> {
        // Phase 2+ implementation:
        // 1. Feed packet data to libvpx decoder
        // 2. Extract decoded frame
        // 3. Convert to vtome Frame format (YUV planes)
        //
        // Status: Requires vpx-sys dependency and C FFI

        Ok(None)
    }

    fn flush(&mut self) -> Result<Vec<Frame>> {
        Ok(Vec::new())
    }

    fn reset(&mut self) -> Result<()> {
        Ok(())
    }

    fn is_hardware(&self) -> bool {
        false
    }
}
