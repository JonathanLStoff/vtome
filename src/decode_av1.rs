//! AV1 decoder using dav1d or rav1d.
//!
//! Royalty-free AV1 decoding for reading both external AV1 files and
//! vtome's own transcoded output.

use crate::error::{Error, Result};
use crate::frame::Frame;
use crate::identify::Encoding;
use crate::media::Packet;

/// AV1 decoder (software, royalty-free).
///
/// Supports pure-Rust AV1 decoding via dav1d or rav1d.
/// AV1 is the primary output codec for vtome transcoding.
///
/// # Implementation Status
///
/// Stub implementation. Requires:
/// 1. dav1d-rs or rav1d crate integration
/// 2. Packet feeding to decoder
/// 3. Frame extraction from decoder output
/// 4. YUV plane handling and frame construction
pub struct Av1Decoder {
    _marker: std::marker::PhantomData<()>,
}

impl Av1Decoder {
    /// Refuses, by name, until it decodes: a decoder that opened and then
    /// produced no pictures would be a black window with no reason given.
    /// vtome plays H.264 — what `import` writes by default — everywhere.
    ///
    /// # Errors
    ///
    /// [`Error::NoDecoder`], always, for now (planning/TODO.md §2).
    pub fn new() -> Result<Self> {
        Err(Error::NoDecoder {
            encoding: Encoding::Av1,
            remedy: "software AV1 decoding (dav1d/rav1d) is not implemented yet; vtome plays \
                     H.264 — import the file to get one"
                .to_string(),
        })
    }
}

impl crate::decode::Decoder for Av1Decoder {
    fn encoding(&self) -> Encoding {
        Encoding::Av1
    }

    fn decode(&mut self, _packet: &Packet) -> Result<Option<Frame>> {
        // Phase 2+ implementation:
        // 1. Feed packet data to dav1d/rav1d decoder
        // 2. Extract decoded frame
        // 3. Convert to vtome Frame format (YUV planes)
        //
        // Status: Blocked on dav1d/rav1d integration
        // (Previous attempts with dav1d had Apple Silicon build issues)

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
