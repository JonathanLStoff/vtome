//! VideoToolbox decoder for macOS and iOS.
//!
//! Uses Apple's VideoToolbox framework for hardware-accelerated H.264, HEVC, and AV1 decoding.
//! Frames come back as CVPixelBuffer objects (can be mapped to Metal textures with no copy).

use std::sync::{Arc, Mutex};
use crate::error::{Error, Result};
use crate::frame::Frame;
use crate::identify::Encoding;
use crate::media::Packet;

// MARK: - VideoToolboxDecoder Implementation

/// VideoToolbox hardware decoder for macOS/iOS.
///
/// Handles H.264, HEVC, and AV1 on devices with hardware support.
/// Frames arrive as CVPixelBuffer which can be directly imported to Metal/wgpu
/// textures with zero copy.
///
/// # Implementation Status
///
/// This decoder provides the infrastructure for VideoToolbox FFI.
/// The actual Objective-C FFI calls are documented and ready for implementation
/// via the objc2 crate.
pub struct VideoToolboxDecoder {
    encoding: Encoding,
    width: u32,
    height: u32,
    /// VTDecompressionSession handle (opaque C pointer)
    session: Option<*mut std::ffi::c_void>,
    /// Decoded frames queue (filled by VTDecompressionSession callback)
    frame_queue: Arc<Mutex<Vec<Frame>>>,
    /// CMVideoFormatDescription (opaque C pointer)
    format_desc: Option<*mut std::ffi::c_void>,
    /// Session initialization status
    session_initialized: bool,
    /// Frame counter for PTS generation
    frame_count: u32,
}

impl VideoToolboxDecoder {
    /// Create a new VideoToolbox decoder for the given encoding.
    pub fn new(encoding: Encoding, width: u32, height: u32) -> Result<Self> {
        match encoding {
            Encoding::H264 | Encoding::H265 | Encoding::Av1 => {
                Ok(VideoToolboxDecoder {
                    encoding,
                    width,
                    height,
                    session: None,
                    frame_queue: Arc::new(Mutex::new(Vec::new())),
                    format_desc: None,
                    session_initialized: false,
                    frame_count: 0,
                })
            }
            _ => Err(Error::Unsupported {
                what: format!("VideoToolbox does not support {}", encoding),
            }),
        }
    }

    /// Initialize the VideoToolbox decompression session from codec extradata.
    ///
    /// Called once with the avcC/hvcC/av1C box from the demuxer.
    fn init_session(&mut self, extra_data: &[u8]) -> Result<()> {
        if self.session_initialized {
            return Ok(());
        }

        if extra_data.is_empty() {
            return Err(Error::Unsupported {
                what: "missing codec extradata (avcC/hvcC)".to_string(),
            });
        }

        // Initialize based on codec
        match self.encoding {
            Encoding::H264 => self.init_h264_session(extra_data)?,
            Encoding::H265 => self.init_hevc_session(extra_data)?,
            Encoding::Av1 => self.init_av1_session(extra_data)?,
            _ => {
                return Err(Error::Unsupported {
                    what: "unsupported encoding".to_string(),
                });
            }
        }

        self.session_initialized = true;
        Ok(())
    }

    /// Initialize H.264 decoder from avcC data.
    ///
    /// **FFI Calls Needed**:
    /// 1. Parse avcC to extract SPS and PPS
    /// 2. CMVideoFormatDescriptionCreateFromH264ParameterSets()
    /// 3. VTDecompressionSessionCreate() with output callback
    fn init_h264_session(&mut self, _avc_data: &[u8]) -> Result<()> {
        // Phase 2+ Implementation:
        // This would parse the avcC box structure and call the FFI functions
        // For now, stub indicates where this would go
        Ok(())
    }

    /// Initialize HEVC decoder from hvcC data.
    fn init_hevc_session(&mut self, _hevc_data: &[u8]) -> Result<()> {
        // Phase 2+ Implementation:
        // Similar to H.264 but with HEVC-specific parameter sets
        Ok(())
    }

    /// Initialize AV1 decoder from av1C data.
    fn init_av1_session(&mut self, _av1_data: &[u8]) -> Result<()> {
        // Phase 2+ Implementation:
        // AV1 configuration and parameter handling
        Ok(())
    }
}

impl crate::decode::Decoder for VideoToolboxDecoder {
    fn encoding(&self) -> Encoding {
        self.encoding
    }

    fn decode(&mut self, packet: &Packet) -> Result<Option<Frame>> {
        // Initialize session on first packet
        // In real implementation, extradata would come from track info in demuxer
        if !self.session_initialized {
            self.init_session(&[])?;
        }

        // Phase 2+ FFI Implementation:
        // 1. Create CMBlockBuffer from packet.data
        //    CMBlockBufferCreateWithMemoryBlock(
        //        allocator, data, size, customBlockSource, ...)
        //
        // 2. Create CMSampleBuffer with timing info
        //    CMSampleBufferCreate(..., blockBuffer, ...)
        //
        // 3. Feed to decoder
        //    VTDecompressionSessionDecodeFrame(session, sampleBuffer, ...)
        //
        // 4. Callback will populate frame_queue with decoded CVPixelBuffer
        //
        // 5. Convert CVPixelBuffer → Frame
        //    - Get dimensions from buffer
        //    - Extract plane data with CVPixelBufferLockBaseAddress
        //    - Create Frame from plane data

        // For now, return any queued frames from previous decode calls
        if let Ok(mut queue) = self.frame_queue.lock() {
            if !queue.is_empty() {
                self.frame_count += 1;
                return Ok(Some(queue.remove(0)));
            }
        }

        Ok(None)
    }

    fn flush(&mut self) -> Result<Vec<Frame>> {
        // Phase 2+ FFI Implementation:
        // Call VTDecompressionSessionFinishDelayedFrames(session)
        // to retrieve any buffered frames (B-frames can delay output)

        if let Ok(mut queue) = self.frame_queue.lock() {
            let frames = queue.drain(..).collect();
            Ok(frames)
        } else {
            Ok(Vec::new())
        }
    }

    fn reset(&mut self) -> Result<()> {
        // Phase 2+ FFI Implementation:
        // Call VTDecompressionSessionInvalidate(session)
        // and CFRelease(session) to clean up for seeking

        if let Ok(mut queue) = self.frame_queue.lock() {
            queue.clear();
        }

        self.session = None;
        self.format_desc = None;
        self.session_initialized = false;
        self.frame_count = 0;

        Ok(())
    }

    fn is_hardware(&self) -> bool {
        true
    }
}

impl Drop for VideoToolboxDecoder {
    fn drop(&mut self) {
        // Phase 2+ FFI Cleanup:
        // Call CFRelease() on session and format_desc pointers
        // Call VTDecompressionSessionInvalidate() if needed

        if self.session.is_some() {
            // Would call VTDecompressionSessionInvalidate + CFRelease here
            self.session = None;
        }

        if self.format_desc.is_some() {
            // Would call CFRelease here
            self.format_desc = None;
        }
    }
}

// MARK: - Thread Safety

// VideoToolbox sessions are thread-safe for multi-threaded decoding
unsafe impl Send for VideoToolboxDecoder {}

// MARK: - VideoToolbox FFI Documentation

/// # VideoToolbox FFI Implementation Guide
///
/// This module provides the structure for VideoToolbox integration.
/// The following are the key Objective-C FFI calls needed:
///
/// ## Format Description Creation
///
/// ```c
/// OSStatus CMVideoFormatDescriptionCreateFromH264ParameterSets(
///     CFAllocatorRef allocator,
///     size_t parameterSetCount,
///     const uint8_t * const *parameterSetPointers,
///     const size_t *parameterSetSizes,
///     int nalUnitHeaderLength,
///     CMVideoFormatDescriptionRef *outDesc);
/// ```
///
/// Extracts SPS/PPS from avcC box and creates format description.
/// This is needed before creating a decompression session.
///
/// ## Decompression Session Creation
///
/// ```c
/// OSStatus VTDecompressionSessionCreate(
///     CFAllocatorRef allocator,
///     CMVideoFormatDescriptionRef videoFormatDescription,
///     CFDictionaryRef videoDecoderSpecification,
///     CFDictionaryRef destinationPixelBufferAttributes,
///     const VTDecompressionOutputCallbackRecord *outputCallback,
///     VTDecompressionSessionRef *decompressionSessionOut);
/// ```
///
/// Creates asynchronous decoder session.
/// Output callback is invoked when frames are decoded.
///
/// ## Frame Decoding
///
/// ```c
/// OSStatus VTDecompressionSessionDecodeFrame(
///     VTDecompressionSessionRef session,
///     CMSampleBufferRef sampleBuffer,
///     VTDecodeFrameFlags decodeFlags,
///     void *sourceFrameRefCon,
///     VTDecodeInfoFlags *infoFlagsOut);
/// ```
///
/// Feeds NAL units to decoder. Asynchronous — callback happens on completion.
///
/// ## Flushing Buffered Frames
///
/// ```c
/// OSStatus VTDecompressionSessionFinishDelayedFrames(
///     VTDecompressionSessionRef session);
/// ```
///
/// Required at EOF to retrieve B-frames that were buffered.
///
/// ## Cleanup
///
/// ```c
/// void VTDecompressionSessionInvalidate(VTDecompressionSessionRef session);
/// CFTypeRef CFRelease(CFTypeRef cf);
/// ```
///
/// Clean up session and release Core Foundation references.
///
/// ## Output Callback
///
/// ```c
/// typedef void (*VTDecompressionOutputCallback)(
///     void *decompressionOutputRefCon,
///     CFDictionaryRef frameInfo,
///     OSStatus status,
///     VTDecodeInfoFlags infoFlags,
///     CVImageBufferRef imageBuffer);
/// ```
///
/// Callback receives decoded CVPixelBuffer (NV12 or similar).
/// This is where decoded frames would be queued for rendering.
///
/// ## CVPixelBuffer Access
///
/// ```c
/// // Get frame properties
/// OSType CVPixelBufferGetPixelFormatType(CVPixelBufferRef pixelBuffer);
/// size_t CVPixelBufferGetWidth(CVPixelBufferRef pixelBuffer);
/// size_t CVPixelBufferGetHeight(CVPixelBufferRef pixelBuffer);
/// size_t CVPixelBufferGetPlaneCount(CVPixelBufferRef pixelBuffer);
///
/// // Access plane data
/// void* CVPixelBufferGetBaseAddressOfPlane(
///     CVPixelBufferRef pixelBuffer, size_t planeIndex);
/// size_t CVPixelBufferGetBytesPerRowOfPlane(
///     CVPixelBufferRef pixelBuffer, size_t planeIndex);
///
/// // Lock/unlock for safe access
/// OSStatus CVPixelBufferLockBaseAddress(
///     CVPixelBufferRef pixelBuffer, CVPixelBufferLockFlags lockingFlags);
/// OSStatus CVPixelBufferUnlockBaseAddress(
///     CVPixelBufferRef pixelBuffer, CVPixelBufferLockFlags lockingFlags);
/// ```
///
/// These would be called to read decoded frame data and convert to vtome Frame format.
///
/// ## Implementation Steps
///
/// 1. **Session Init** (in `init_h264_session`):
///    - Parse avcC/hvcC box to extract SPS/PPS
///    - Call CMVideoFormatDescriptionCreateFromH264ParameterSets
///    - Call VTDecompressionSessionCreate with callback
///    - Store session pointer
///
/// 2. **Decoding** (in `decode`):
///    - Create CMBlockBuffer from packet data
///    - Create CMSampleBuffer with timing
///    - Call VTDecompressionSessionDecodeFrame
///    - Callback populates frame_queue
///    - Return frame from queue
///
/// 3. **CVPixelBuffer → Frame**:
///    - Lock base address
///    - Read pixel format (NV12, I420, etc.)
///    - Extract plane data and strides
///    - Create Frame with YUV planes
///    - Unlock base address
///
/// 4. **Cleanup** (in `drop`):
///    - Call VTDecompressionSessionInvalidate
///    - Call CFRelease on all CF references
