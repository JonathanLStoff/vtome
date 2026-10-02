//! VideoToolbox, the other way: H.264 through the encoder Apple ships.
//!
//! The only way vtome writes H.264. The encoder is the operating system's —
//! the media engine on Apple silicon, or Apple's software encoder on a Mac
//! when asked — so the patent licence is Apple's, exactly as it is for
//! decoding. Nothing here bundles an H.264 encoder.
//!
//! Like [`crate::decode_videotoolbox`], this is plain `extern "C"` into
//! VideoToolbox, CoreMedia, CoreVideo, and CoreFoundation, declared by hand
//! and checked against the SDK headers.
//!
//! # The settings are the spec
//!
//! `planning/TODO.md` §15 is the contract every file must meet so any
//! hardware decoder takes it instantly, and each part of it is a property set
//! here:
//!
//! | §15 | Here |
//! |---|---|
//! | High profile, Level 4.1 — declared, never AutoLevel | `ProfileLevel = H264_High_4_1` (the caller has already scaled the picture to fit 4.1) |
//! | CABAC | `H264EntropyMode = CABAC` |
//! | No B-frames | `AllowFrameReordering = false` |
//! | Closed GOP, a keyframe every 2 s exactly | `MaxKeyFrameInterval` and `…Duration` at 2 s, *and* every 2 s'th frame forced to a keyframe, so the cadence is the spec's and not the encoder's |
//! | Quality 0.65–0.70 | `Quality = 0.68`, or a bitrate where the encoder will not take a quality |
//! | 8-bit 4:2:0 | NV12 in, `420v` or `420f` by range |
//!
//! # Asynchronous, in order
//!
//! VideoToolbox may finish a frame after `EncodeFrame` returns, on a thread of
//! its own, so packets are collected behind a mutex and handed out on the next
//! call. With frame reordering off they arrive in the order the pictures went
//! in.

use std::ffi::{c_int, c_void};
use std::ptr;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use crate::bitstream::AvcConfig;
use crate::color::{ColorSpace, Matrix, Primaries, Range, Transfer};
use crate::decode::Hardware;
use crate::encode::{Backend, Encoder, EncoderConfig, Level};
use crate::error::{Error, Result};
use crate::frame::Frame;
use crate::identify::Encoding;
use crate::media::Packet;

/// The C side, declared by hand.
#[allow(non_upper_case_globals, non_snake_case, dead_code)]
mod sys {
    use std::ffi::{c_int, c_void};
    use std::marker::{PhantomData, PhantomPinned};

    pub type OSStatus = i32;
    pub type CFTypeRef = *const c_void;
    pub type CFAllocatorRef = *const c_void;
    pub type CFDictionaryRef = *const c_void;
    pub type CFArrayRef = *const c_void;
    pub type CFStringRef = *const c_void;
    pub type CFNumberRef = *const c_void;
    pub type CFBooleanRef = *const c_void;
    pub type CFIndex = isize;
    pub type CMFormatDescriptionRef = *const c_void;
    pub type CMBlockBufferRef = *const c_void;
    pub type CMSampleBufferRef = *const c_void;
    pub type CVPixelBufferRef = *const c_void;
    pub type CVPixelBufferPoolRef = *const c_void;
    pub type VTCompressionSessionRef = *const c_void;

    pub const DEFAULT_ALLOCATOR: CFAllocatorRef = std::ptr::null();

    pub const kCFNumberSInt32Type: CFIndex = 3;
    pub const kCFNumberFloat64Type: CFIndex = 6;
    pub const kCMTimeFlags_Valid: u32 = 1 << 0;
    pub const kVTEncodeInfo_FrameDropped: u32 = 1 << 1;
    /// `kVTPropertyNotSupportedErr`: this encoder does not take that setting.
    pub const kVTPropertyNotSupportedErr: OSStatus = -12900;
    /// `'avc1'`.
    pub const kCMVideoCodecType_H264: u32 = u32::from_be_bytes(*b"avc1");

    #[repr(C)]
    #[derive(Clone, Copy, Debug)]
    pub struct CMTime {
        pub value: i64,
        pub timescale: i32,
        pub flags: u32,
        pub epoch: i64,
    }

    pub const INVALID_TIME: CMTime = CMTime {
        value: 0,
        timescale: 0,
        flags: 0,
        epoch: 0,
    };

    pub type VTCompressionOutputCallback = unsafe extern "C" fn(
        output_ref_con: *mut c_void,
        source_frame_ref_con: *mut c_void,
        status: OSStatus,
        info_flags: u32,
        sample_buffer: CMSampleBufferRef,
    );

    #[repr(C)]
    pub struct CFDictionaryCallBacks {
        _opaque: [u8; 0],
        _marker: PhantomData<(*mut u8, PhantomPinned)>,
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        pub static kCFTypeDictionaryKeyCallBacks: CFDictionaryCallBacks;
        pub static kCFTypeDictionaryValueCallBacks: CFDictionaryCallBacks;
        pub static kCFBooleanTrue: CFBooleanRef;
        pub static kCFBooleanFalse: CFBooleanRef;

        pub fn CFDictionaryCreate(
            allocator: CFAllocatorRef,
            keys: *const *const c_void,
            values: *const *const c_void,
            count: CFIndex,
            key_callbacks: *const CFDictionaryCallBacks,
            value_callbacks: *const CFDictionaryCallBacks,
        ) -> CFDictionaryRef;
        pub fn CFDictionaryGetValue(dictionary: CFDictionaryRef, key: *const c_void)
            -> *const c_void;
        pub fn CFNumberCreate(
            allocator: CFAllocatorRef,
            number_type: CFIndex,
            value: *const c_void,
        ) -> CFNumberRef;
        pub fn CFArrayGetCount(array: CFArrayRef) -> CFIndex;
        pub fn CFArrayGetValueAtIndex(array: CFArrayRef, index: CFIndex) -> *const c_void;
        pub fn CFBooleanGetValue(boolean: CFBooleanRef) -> u8;
        pub fn CFRelease(object: CFTypeRef);
    }

    #[link(name = "CoreMedia", kind = "framework")]
    extern "C" {
        pub static kCMSampleAttachmentKey_NotSync: CFStringRef;

        pub fn CMSampleBufferGetDataBuffer(sample: CMSampleBufferRef) -> CMBlockBufferRef;
        pub fn CMSampleBufferGetPresentationTimeStamp(sample: CMSampleBufferRef) -> CMTime;
        pub fn CMSampleBufferGetDecodeTimeStamp(sample: CMSampleBufferRef) -> CMTime;
        pub fn CMSampleBufferGetFormatDescription(sample: CMSampleBufferRef)
            -> CMFormatDescriptionRef;
        pub fn CMSampleBufferGetSampleAttachmentsArray(
            sample: CMSampleBufferRef,
            create_if_necessary: u8,
        ) -> CFArrayRef;
        pub fn CMBlockBufferGetDataLength(buffer: CMBlockBufferRef) -> usize;
        pub fn CMBlockBufferCopyDataBytes(
            buffer: CMBlockBufferRef,
            offset: usize,
            length: usize,
            destination: *mut c_void,
        ) -> OSStatus;
        pub fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
            description: CMFormatDescriptionRef,
            index: usize,
            set_out: *mut *const u8,
            size_out: *mut usize,
            count_out: *mut usize,
            nal_header_length_out: *mut c_int,
        ) -> OSStatus;
    }

    #[link(name = "CoreVideo", kind = "framework")]
    extern "C" {
        pub static kCVPixelBufferPixelFormatTypeKey: CFStringRef;
        pub static kCVPixelBufferWidthKey: CFStringRef;
        pub static kCVPixelBufferHeightKey: CFStringRef;

        pub static kCVImageBufferColorPrimaries_ITU_R_709_2: CFStringRef;
        pub static kCVImageBufferColorPrimaries_EBU_3213: CFStringRef;
        pub static kCVImageBufferColorPrimaries_SMPTE_C: CFStringRef;
        pub static kCVImageBufferColorPrimaries_ITU_R_2020: CFStringRef;
        pub static kCVImageBufferTransferFunction_ITU_R_709_2: CFStringRef;
        pub static kCVImageBufferYCbCrMatrix_ITU_R_709_2: CFStringRef;
        pub static kCVImageBufferYCbCrMatrix_ITU_R_601_4: CFStringRef;
        pub static kCVImageBufferYCbCrMatrix_ITU_R_2020: CFStringRef;

        pub fn CVPixelBufferPoolCreatePixelBuffer(
            allocator: CFAllocatorRef,
            pool: CVPixelBufferPoolRef,
            buffer_out: *mut CVPixelBufferRef,
        ) -> i32;
        pub fn CVPixelBufferLockBaseAddress(buffer: CVPixelBufferRef, flags: u64) -> i32;
        pub fn CVPixelBufferUnlockBaseAddress(buffer: CVPixelBufferRef, flags: u64) -> i32;
        pub fn CVPixelBufferGetBaseAddressOfPlane(buffer: CVPixelBufferRef, plane: usize)
            -> *mut c_void;
        pub fn CVPixelBufferGetBytesPerRowOfPlane(buffer: CVPixelBufferRef, plane: usize) -> usize;
        pub fn CVPixelBufferGetHeightOfPlane(buffer: CVPixelBufferRef, plane: usize) -> usize;
        pub fn CVPixelBufferGetWidthOfPlane(buffer: CVPixelBufferRef, plane: usize) -> usize;
    }

    #[link(name = "VideoToolbox", kind = "framework")]
    extern "C" {
        pub static kVTCompressionPropertyKey_RealTime: CFStringRef;
        pub static kVTCompressionPropertyKey_ProfileLevel: CFStringRef;
        pub static kVTProfileLevel_H264_High_4_1: CFStringRef;
        pub static kVTProfileLevel_H264_High_AutoLevel: CFStringRef;
        pub static kVTCompressionPropertyKey_H264EntropyMode: CFStringRef;
        pub static kVTH264EntropyMode_CABAC: CFStringRef;
        pub static kVTCompressionPropertyKey_AllowFrameReordering: CFStringRef;
        pub static kVTCompressionPropertyKey_MaxKeyFrameInterval: CFStringRef;
        pub static kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration: CFStringRef;
        pub static kVTCompressionPropertyKey_ExpectedFrameRate: CFStringRef;
        pub static kVTCompressionPropertyKey_Quality: CFStringRef;
        pub static kVTCompressionPropertyKey_AverageBitRate: CFStringRef;
        pub static kVTCompressionPropertyKey_ColorPrimaries: CFStringRef;
        pub static kVTCompressionPropertyKey_TransferFunction: CFStringRef;
        pub static kVTCompressionPropertyKey_YCbCrMatrix: CFStringRef;
        pub static kVTEncodeFrameOptionKey_ForceKeyFrame: CFStringRef;

        // macOS only, as for the decoder: iOS encodes H.264 in hardware and
        // offers no choice, and the query property arrives there in iOS 17.
        #[cfg(target_os = "macos")]
        pub static kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder: CFStringRef;
        #[cfg(target_os = "macos")]
        pub static kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder: CFStringRef;
        #[cfg(target_os = "macos")]
        pub static kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: CFStringRef;

        pub fn VTCompressionSessionCreate(
            allocator: CFAllocatorRef,
            width: i32,
            height: i32,
            codec_type: u32,
            encoder_specification: CFDictionaryRef,
            source_image_buffer_attributes: CFDictionaryRef,
            compressed_data_allocator: CFAllocatorRef,
            output_callback: VTCompressionOutputCallback,
            output_callback_ref_con: *mut c_void,
            compression_session_out: *mut VTCompressionSessionRef,
        ) -> OSStatus;
        pub fn VTSessionSetProperty(session: CFTypeRef, key: CFStringRef, value: CFTypeRef)
            -> OSStatus;
        pub fn VTSessionCopyProperty(
            session: CFTypeRef,
            key: CFStringRef,
            allocator: CFAllocatorRef,
            value_out: *mut c_void,
        ) -> OSStatus;
        pub fn VTCompressionSessionPrepareToEncodeFrames(session: VTCompressionSessionRef)
            -> OSStatus;
        pub fn VTCompressionSessionGetPixelBufferPool(session: VTCompressionSessionRef)
            -> CVPixelBufferPoolRef;
        pub fn VTCompressionSessionEncodeFrame(
            session: VTCompressionSessionRef,
            image_buffer: CVPixelBufferRef,
            presentation_time_stamp: CMTime,
            duration: CMTime,
            frame_properties: CFDictionaryRef,
            source_frame_ref_con: *mut c_void,
            info_flags_out: *mut u32,
        ) -> OSStatus;
        pub fn VTCompressionSessionCompleteFrames(
            session: VTCompressionSessionRef,
            complete_until_presentation_time_stamp: CMTime,
        ) -> OSStatus;
        pub fn VTCompressionSessionInvalidate(session: VTCompressionSessionRef);
    }
}

/// `kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange`: NV12, limited range.
const NV12_VIDEO_RANGE: u32 = u32::from_be_bytes(*b"420v");
/// `kCVPixelFormatType_420YpCbCr8BiPlanarFullRange`: NV12, full range.
const NV12_FULL_RANGE: u32 = u32::from_be_bytes(*b"420f");

/// One reference to a Core Foundation object, released when dropped.
struct Retained(sys::CFTypeRef);

impl Retained {
    /// # Safety
    ///
    /// `object` must be null or a +1 reference that nothing else will release.
    unsafe fn adopt(object: sys::CFTypeRef) -> Option<Self> {
        (!object.is_null()).then_some(Retained(object))
    }

    fn get(&self) -> sys::CFTypeRef {
        self.0
    }
}

impl Drop for Retained {
    fn drop(&mut self) {
        // SAFETY: `adopt` only accepts a reference this value owns outright.
        unsafe { sys::CFRelease(self.0) }
    }
}

/// What the output callback writes into. Boxed by the encoder so its address
/// stays put: the session holds it for its whole life.
#[derive(Default)]
struct Output {
    packets: Vec<Packet>,
    /// The SPS and PPS, from the first frame's format description.
    parameter_sets: Option<(Vec<u8>, Vec<u8>)>,
    failure: Option<String>,
}

/// H.264 through VideoToolbox, to §15's spec.
pub struct VideoToolboxEncoder {
    session: Retained,
    output: Box<Mutex<Output>>,
    /// `{ ForceKeyFrame: true }`, handed with every keyframe-to-be.
    force_keyframe: Retained,
    keyframe_frames: u64,
    frames_in: u64,
    width: u32,
    height: u32,
    full_range: bool,
    hardware: bool,
    record: Option<Vec<u8>>,
}

// SAFETY: compression sessions and the CF objects here may be used from any
// thread, one at a time; `&mut self` on every call that touches the session
// guarantees that, and the callback's state is behind a mutex.
unsafe impl Send for VideoToolboxEncoder {}

impl VideoToolboxEncoder {
    /// An H.264 encoder for `config`.
    ///
    /// # Errors
    ///
    /// [`Error::NoEncoder`] for anything but H.264, for software asked for on
    /// a platform that encodes only in hardware, and for hardware required
    /// where the session landed in software; [`Error::Encode`] if
    /// VideoToolbox will not open or configure a session.
    pub fn new(config: &EncoderConfig) -> Result<Self> {
        if config.encoding != Encoding::H264 {
            return Err(Error::NoEncoder {
                encoding: config.encoding,
                remedy: "VideoToolbox writes H.264 for vtome; AV1 is rav1e's".to_string(),
            });
        }

        let (width, height) = (
            i32::try_from(config.width).map_err(|_| encode_error("the picture is too wide"))?,
            i32::try_from(config.height).map_err(|_| encode_error("the picture is too tall"))?,
        );

        let full_range = config.color.range == Range::Full;
        let specification = encoder_specification(config.hardware)?;
        let attributes = source_attributes(config.width, config.height, full_range)?;

        let output = Box::new(Mutex::new(Output::default()));
        let mut session = ptr::null();

        // SAFETY: every reference is live for the call; `output` is boxed, so
        // the address handed over stays valid until the session is
        // invalidated in `drop`. `session` receives a +1 reference.
        let status = unsafe {
            sys::VTCompressionSessionCreate(
                sys::DEFAULT_ALLOCATOR,
                width,
                height,
                sys::kCMVideoCodecType_H264,
                specification.as_ref().map_or(ptr::null(), Retained::get),
                attributes.get(),
                sys::DEFAULT_ALLOCATOR,
                on_encoded,
                ptr::from_ref::<Mutex<Output>>(&output).cast_mut().cast(),
                &mut session,
            )
        };
        check(status, "opening a compression session")?;

        // SAFETY: a `Create` call's +1 reference.
        let session = unsafe { Retained::adopt(session) }
            .ok_or_else(|| encode_error("VideoToolbox opened no session"))?;

        configure(&session, config)?;

        // SAFETY: the session is live.
        check(
            unsafe { sys::VTCompressionSessionPrepareToEncodeFrames(session.get()) },
            "preparing to encode",
        )?;

        let hardware = uses_hardware(&session);

        if config.hardware == Hardware::Require && !hardware {
            // SAFETY: the session is live and is not used again.
            unsafe { sys::VTCompressionSessionInvalidate(session.get()) };

            return Err(Error::NoEncoder {
                encoding: Encoding::H264,
                remedy: "hardware encoding was required, and VideoToolbox opened a software \
                         session — the media engine is busy or this machine has none"
                    .to_string(),
            });
        }

        // SAFETY: immutable framework constants.
        let force_keyframe = dictionary(&[(
            unsafe { sys::kVTEncodeFrameOptionKey_ForceKeyFrame },
            unsafe { sys::kCFBooleanTrue },
        )])?;

        Ok(VideoToolboxEncoder {
            session,
            output,
            force_keyframe,
            keyframe_frames: u64::from(config.keyframe_frames()),
            frames_in: 0,
            width: config.width,
            height: config.height,
            full_range,
            hardware,
            record: None,
        })
    }

    /// Takes what the callback has produced, turning a failure into an error.
    fn collect(&mut self) -> Result<Vec<Packet>> {
        let mut output = self.output.lock().unwrap_or_else(PoisonError::into_inner);

        if let Some(reason) = output.failure.take() {
            return Err(encode_error(format!("encoding a frame: {reason}")));
        }

        if self.record.is_none() {
            if let Some((sps, pps)) = &output.parameter_sets {
                self.record = Some(AvcConfig::new(sps.clone(), pps.clone()).to_record());
            }
        }

        Ok(std::mem::take(&mut output.packets))
    }

    /// A pixel buffer from the session's pool, with `frame`'s planes copied in.
    fn pixel_buffer(&self, frame: &Frame) -> Result<Retained> {
        let nv12 = crate::scale::to_nv12(frame)?;

        // SAFETY: the session is live; the pool is the session's (a Get, not
        // retained) and outlives this call.
        let pool = unsafe { sys::VTCompressionSessionGetPixelBufferPool(self.session.get()) };
        if pool.is_null() {
            return Err(encode_error("the session has no pixel buffer pool"));
        }

        let mut buffer = ptr::null();
        // SAFETY: `buffer` receives a +1 reference.
        let status = unsafe {
            sys::CVPixelBufferPoolCreatePixelBuffer(sys::DEFAULT_ALLOCATOR, pool, &mut buffer)
        };
        check(status, "taking a pixel buffer from the pool")?;
        // SAFETY: a `Create` call's +1 reference.
        let buffer = unsafe { Retained::adopt(buffer) }
            .ok_or_else(|| encode_error("the pool gave no pixel buffer"))?;

        // SAFETY: a live pixel buffer, locked for writing.
        if unsafe { sys::CVPixelBufferLockBaseAddress(buffer.get(), 0) } != 0 {
            return Err(encode_error("a pixel buffer could not be locked for writing"));
        }

        /// Unlocks on every way out.
        struct Unlock(sys::CVPixelBufferRef);

        impl Drop for Unlock {
            fn drop(&mut self) {
                // SAFETY: locked above with the same flags.
                unsafe {
                    sys::CVPixelBufferUnlockBaseAddress(self.0, 0);
                }
            }
        }

        let _unlock = Unlock(buffer.get());

        for plane in 0..2 {
            // SAFETY: the buffer is locked; these describe its own planes.
            let (base, stride, rows, width) = unsafe {
                (
                    sys::CVPixelBufferGetBaseAddressOfPlane(buffer.get(), plane).cast::<u8>(),
                    sys::CVPixelBufferGetBytesPerRowOfPlane(buffer.get(), plane),
                    sys::CVPixelBufferGetHeightOfPlane(buffer.get(), plane),
                    sys::CVPixelBufferGetWidthOfPlane(buffer.get(), plane),
                )
            };

            if base.is_null() {
                return Err(encode_error("a locked plane had no address"));
            }

            // Plane 1 is interleaved UV: two bytes a chroma sample.
            let row_bytes = width * if plane == 0 { 1 } else { 2 };

            for row in 0..rows {
                let Some(source) = nv12.row(plane, row as u32) else {
                    break;
                };
                let length = source.len().min(row_bytes).min(stride);

                // SAFETY: `row < rows` and `length <= stride`, so this stays
                // inside the locked plane; the source is a live slice.
                unsafe {
                    ptr::copy_nonoverlapping(source.as_ptr(), base.add(row * stride), length);
                }
            }
        }

        Ok(buffer)
    }
}

impl Encoder for VideoToolboxEncoder {
    fn encoding(&self) -> Encoding {
        Encoding::H264
    }

    fn backend(&self) -> Backend {
        Backend::VideoToolbox
    }

    fn is_hardware(&self) -> bool {
        self.hardware
    }

    fn config_record(&self) -> Option<Vec<u8>> {
        self.record.clone()
    }

    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>> {
        if (frame.width(), frame.height()) != (self.width, self.height) {
            return Err(encode_error(format!(
                "a {}×{} picture handed to an encoder opened for {}×{}",
                frame.width(),
                frame.height(),
                self.width,
                self.height
            )));
        }

        if (frame.color().range == Range::Full) != self.full_range {
            return Err(encode_error(
                "a picture whose range differs from the one the encoder was opened for",
            ));
        }

        let buffer = self.pixel_buffer(frame)?;

        // The spec's cadence, not the encoder's: every keyframe_frames'th
        // picture is forced to be one, and the interval caps set at open stop
        // any in between.
        let properties = (self.frames_in % self.keyframe_frames == 0)
            .then(|| self.force_keyframe.get())
            .unwrap_or(ptr::null());
        self.frames_in += 1;

        let mut flags = 0_u32;

        // SAFETY: the session and buffer are live for the call; the session
        // retains the buffer for as long as it needs it.
        let status = unsafe {
            sys::VTCompressionSessionEncodeFrame(
                self.session.get(),
                buffer.get(),
                to_cm_time(frame.pts()),
                sys::INVALID_TIME,
                properties,
                ptr::null_mut(),
                &mut flags,
            )
        };
        check(status, "encoding a frame")?;

        self.collect()
    }

    fn finish(&mut self) -> Result<Vec<Packet>> {
        // SAFETY: the session is live until `drop`; an invalid time means
        // "everything".
        let status = unsafe {
            sys::VTCompressionSessionCompleteFrames(self.session.get(), sys::INVALID_TIME)
        };
        check(status, "finishing the stream")?;

        self.collect()
    }
}

impl Drop for VideoToolboxEncoder {
    fn drop(&mut self) {
        // After this no callback can arrive, so `output` may go.
        // SAFETY: the session is live, and this is the last use of it.
        unsafe { sys::VTCompressionSessionInvalidate(self.session.get()) }
    }
}

/// Every property §15 asks for, and the colour the pictures carry.
fn configure(session: &Retained, config: &EncoderConfig) -> Result<()> {
    let set = |key: sys::CFStringRef, value: sys::CFTypeRef, what: &str| {
        // SAFETY: a live session, a framework key, and a live value the
        // session retains if it keeps it.
        check(
            unsafe { sys::VTSessionSetProperty(session.get(), key, value) },
            &format!("setting {what}"),
        )
    };

    let keyframes = number_i32(i32::try_from(config.keyframe_frames()).unwrap_or(i32::MAX))?;
    let keyframe_seconds = number_f64(config.keyframe_interval.as_secs_f64())?;
    let frame_rate = number_f64(config.frame_rate.as_f64())?;

    // SAFETY: immutable framework constants, read once each.
    unsafe {
        let profile = match config.level {
            Level::L4_1 => sys::kVTProfileLevel_H264_High_4_1,
            Level::Auto => sys::kVTProfileLevel_H264_High_AutoLevel,
        };

        set(sys::kVTCompressionPropertyKey_ProfileLevel, profile, "the profile and level")?;
        set(
            sys::kVTCompressionPropertyKey_H264EntropyMode,
            sys::kVTH264EntropyMode_CABAC,
            "CABAC",
        )?;
        set(
            sys::kVTCompressionPropertyKey_AllowFrameReordering,
            sys::kCFBooleanFalse,
            "no B-frames",
        )?;
        set(
            sys::kVTCompressionPropertyKey_MaxKeyFrameInterval,
            keyframes.get(),
            "the keyframe interval",
        )?;
        set(
            sys::kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
            keyframe_seconds.get(),
            "the keyframe interval in seconds",
        )?;
        set(
            sys::kVTCompressionPropertyKey_RealTime,
            if config.realtime {
                sys::kCFBooleanTrue
            } else {
                sys::kCFBooleanFalse
            },
            "real-time",
        )?;

        // A hint only; some encoders decline it, and nothing depends on it.
        let _ = set(
            sys::kVTCompressionPropertyKey_ExpectedFrameRate,
            frame_rate.get(),
            "the frame rate",
        );

        // Quality where the encoder takes one; a bitrate for the same
        // quality where it does not.
        let quality = number_f64(f64::from(config.quality.clamp(0.0, 1.0)))?;
        let status = sys::VTSessionSetProperty(
            session.get(),
            sys::kVTCompressionPropertyKey_Quality,
            quality.get(),
        );

        if status == sys::kVTPropertyNotSupportedErr {
            let bitrate = number_i32(i32::try_from(config.bitrate()).unwrap_or(i32::MAX))?;
            set(
                sys::kVTCompressionPropertyKey_AverageBitRate,
                bitrate.get(),
                "the bitrate",
            )?;
        } else {
            check(status, "setting the quality")?;
        }

        for (key, value) in color_properties(config.color) {
            // Colour tags are written into the SPS for the decoder's sake;
            // an encoder that declines one still encodes correctly.
            let _ = set(key, value, "a colour tag");
        }
    }

    Ok(())
}

/// The session properties that tag the stream with `color`, as far as
/// CoreVideo has a name for it.
///
/// # Safety
///
/// Reads framework constants; call only where those are linked.
unsafe fn color_properties(color: ColorSpace) -> Vec<(sys::CFStringRef, sys::CFTypeRef)> {
    let mut properties = Vec::with_capacity(3);

    let matrix = match color.matrix {
        Matrix::Bt709 => Some(sys::kCVImageBufferYCbCrMatrix_ITU_R_709_2),
        Matrix::Bt601 => Some(sys::kCVImageBufferYCbCrMatrix_ITU_R_601_4),
        Matrix::Bt2020Ncl => Some(sys::kCVImageBufferYCbCrMatrix_ITU_R_2020),
        Matrix::Identity => None,
    };

    let primaries = match color.primaries {
        Primaries::Bt709 => sys::kCVImageBufferColorPrimaries_ITU_R_709_2,
        Primaries::Bt601_625 => sys::kCVImageBufferColorPrimaries_EBU_3213,
        Primaries::Bt601_525 => sys::kCVImageBufferColorPrimaries_SMPTE_C,
        Primaries::Bt2020 => sys::kCVImageBufferColorPrimaries_ITU_R_2020,
    };

    // §15 is SDR; the HDR curves are left unstated rather than declared on a
    // stream that was never tone-mapped for them.
    let transfer = (color.transfer == Transfer::Bt709)
        .then_some(sys::kCVImageBufferTransferFunction_ITU_R_709_2);

    if let Some(matrix) = matrix {
        properties.push((sys::kVTCompressionPropertyKey_YCbCrMatrix, matrix));
    }
    properties.push((sys::kVTCompressionPropertyKey_ColorPrimaries, primaries));
    if let Some(transfer) = transfer {
        properties.push((sys::kVTCompressionPropertyKey_TransferFunction, transfer));
    }

    properties
}

/// What to ask VideoToolbox for: nothing for "whichever", or a dictionary that
/// requires or rules out the hardware encoder.
fn encoder_specification(hardware: Hardware) -> Result<Option<Retained>> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: immutable framework constants.
        let pair = unsafe {
            match hardware {
                Hardware::Prefer => return Ok(None),
                Hardware::Require => (
                    sys::kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
                    sys::kCFBooleanTrue,
                ),
                Hardware::Off => (
                    sys::kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
                    sys::kCFBooleanFalse,
                ),
            }
        };

        dictionary(&[pair]).map(Some)
    }

    #[cfg(not(target_os = "macos"))]
    match hardware {
        Hardware::Off => Err(Error::NoEncoder {
            encoding: Encoding::H264,
            remedy: "this platform encodes H.264 only in hardware; software encoding is a \
                     macOS choice"
                .to_string(),
        }),
        _ => Ok(None),
    }
}

/// `{ PixelFormatType: '420v' or '420f', Width, Height }`, so the session's
/// pool makes NV12 buffers of the right size.
fn source_attributes(width: u32, height: u32, full_range: bool) -> Result<Retained> {
    let format = number_i32(if full_range { NV12_FULL_RANGE } else { NV12_VIDEO_RANGE } as i32)?;
    let width = number_i32(i32::try_from(width).unwrap_or(i32::MAX))?;
    let height = number_i32(i32::try_from(height).unwrap_or(i32::MAX))?;

    // SAFETY: immutable framework constants.
    let pairs = unsafe {
        [
            (sys::kCVPixelBufferPixelFormatTypeKey, format.get()),
            (sys::kCVPixelBufferWidthKey, width.get()),
            (sys::kCVPixelBufferHeightKey, height.get()),
        ]
    };

    dictionary(&pairs)
}

/// A CF dictionary of these pairs; the CFType callbacks retain each one.
fn dictionary(pairs: &[(sys::CFStringRef, sys::CFTypeRef)]) -> Result<Retained> {
    let keys: Vec<*const c_void> = pairs.iter().map(|(key, _)| *key).collect();
    let values: Vec<*const c_void> = pairs.iter().map(|(_, value)| *value).collect();

    // SAFETY: `keys` and `values` are the same length and every entry is a
    // live CF object for the call.
    let dictionary = unsafe {
        sys::CFDictionaryCreate(
            sys::DEFAULT_ALLOCATOR,
            keys.as_ptr(),
            values.as_ptr(),
            keys.len() as sys::CFIndex,
            ptr::addr_of!(sys::kCFTypeDictionaryKeyCallBacks),
            ptr::addr_of!(sys::kCFTypeDictionaryValueCallBacks),
        )
    };

    // SAFETY: a `Create` call's +1 reference.
    unsafe { Retained::adopt(dictionary) }.ok_or_else(|| encode_error("CoreFoundation made no dictionary"))
}

fn number_i32(value: i32) -> Result<Retained> {
    // SAFETY: a 32-bit integer read as `kCFNumberSInt32Type`.
    unsafe {
        Retained::adopt(sys::CFNumberCreate(
            sys::DEFAULT_ALLOCATOR,
            sys::kCFNumberSInt32Type,
            ptr::from_ref(&value).cast(),
        ))
    }
    .ok_or_else(|| encode_error("CoreFoundation made no number"))
}

fn number_f64(value: f64) -> Result<Retained> {
    // SAFETY: a double read as `kCFNumberFloat64Type`.
    unsafe {
        Retained::adopt(sys::CFNumberCreate(
            sys::DEFAULT_ALLOCATOR,
            sys::kCFNumberFloat64Type,
            ptr::from_ref(&value).cast(),
        ))
    }
    .ok_or_else(|| encode_error("CoreFoundation made no number"))
}

/// Whether the session landed on the hardware encoder, by asking it.
#[cfg(target_os = "macos")]
fn uses_hardware(session: &Retained) -> bool {
    let mut value: sys::CFBooleanRef = ptr::null();

    // SAFETY: a property this session type defines, copied out as a +1
    // CFBoolean.
    let status = unsafe {
        sys::VTSessionCopyProperty(
            session.get(),
            sys::kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder,
            sys::DEFAULT_ALLOCATOR,
            ptr::from_mut(&mut value).cast(),
        )
    };

    if status != 0 {
        return false;
    }

    // SAFETY: a `Copy` call's +1 reference.
    let Some(value) = (unsafe { Retained::adopt(value) }) else {
        return false;
    };

    // SAFETY: the property is documented as a CFBoolean.
    unsafe { sys::CFBooleanGetValue(value.get()) != 0 }
}

/// Elsewhere VideoToolbox encodes H.264 only in hardware.
#[cfg(not(target_os = "macos"))]
fn uses_hardware(_session: &Retained) -> bool {
    true
}

/// Called by VideoToolbox once per finished picture, on a thread of its own.
unsafe extern "C" fn on_encoded(
    output: *mut c_void,
    _source_frame: *mut c_void,
    status: sys::OSStatus,
    info_flags: u32,
    sample: sys::CMSampleBufferRef,
) {
    // A panic must not unwind into VideoToolbox's stack frames.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: `output` is the boxed mutex the encoder registered, and the
        // encoder invalidates the session before that box is freed.
        let output = unsafe { &*output.cast_const().cast::<Mutex<Output>>() };
        let mut output = output.lock().unwrap_or_else(PoisonError::into_inner);

        if status != 0 {
            output.failure = Some(describe(status));
            return;
        }

        if sample.is_null() || info_flags & sys::kVTEncodeInfo_FrameDropped != 0 {
            return;
        }

        // SAFETY: VideoToolbox guarantees `sample` for the duration of the call.
        match unsafe { packet_from(sample) } {
            Ok(packet) => {
                if output.parameter_sets.is_none() {
                    // SAFETY: as above.
                    output.parameter_sets = unsafe { parameter_sets(sample) };
                }
                output.packets.push(packet);
            }
            Err(error) => output.failure = Some(error.to_string()),
        }
    }));
}

/// One encoded sample as a packet: four-byte length-prefixed NAL units, which
/// is what MP4 stores.
///
/// # Safety
///
/// `sample` must be a live `CMSampleBuffer` for the duration of the call.
unsafe fn packet_from(sample: sys::CMSampleBufferRef) -> Result<Packet> {
    let block = sys::CMSampleBufferGetDataBuffer(sample);
    if block.is_null() {
        return Err(encode_error("an encoded sample with no data"));
    }

    let length = sys::CMBlockBufferGetDataLength(block);
    let mut data = vec![0_u8; length];
    check(
        sys::CMBlockBufferCopyDataBytes(block, 0, length, data.as_mut_ptr().cast()),
        "copying an encoded sample out",
    )?;

    let pts = to_duration(sys::CMSampleBufferGetPresentationTimeStamp(sample));
    let dts = sys::CMSampleBufferGetDecodeTimeStamp(sample);
    // No reordering, so decode time is presentation time; VideoToolbox leaves
    // it invalid to say exactly that.
    let dts = if dts.flags & sys::kCMTimeFlags_Valid == 0 {
        pts
    } else {
        to_duration(dts)
    };

    Ok(Packet {
        track_id: 1,
        data,
        pts,
        dts,
        is_keyframe: is_sync(sample),
    })
}

/// A sample is a keyframe unless its attachments say `NotSync`.
///
/// # Safety
///
/// `sample` must be a live `CMSampleBuffer`.
unsafe fn is_sync(sample: sys::CMSampleBufferRef) -> bool {
    let attachments = sys::CMSampleBufferGetSampleAttachmentsArray(sample, 0);
    if attachments.is_null() || sys::CFArrayGetCount(attachments) == 0 {
        return true;
    }

    let first = sys::CFArrayGetValueAtIndex(attachments, 0);
    if first.is_null() {
        return true;
    }

    let not_sync = sys::CFDictionaryGetValue(first, sys::kCMSampleAttachmentKey_NotSync);

    not_sync.is_null() || sys::CFBooleanGetValue(not_sync) == 0
}

/// The SPS and PPS out of a sample's format description.
///
/// # Safety
///
/// `sample` must be a live `CMSampleBuffer`.
unsafe fn parameter_sets(sample: sys::CMSampleBufferRef) -> Option<(Vec<u8>, Vec<u8>)> {
    let description = sys::CMSampleBufferGetFormatDescription(sample);
    if description.is_null() {
        return None;
    }

    let set = |index: usize| -> Option<Vec<u8>> {
        let (mut pointer, mut size, mut count, mut header) =
            (ptr::null(), 0_usize, 0_usize, 0 as c_int);

        // SAFETY: a live description; the out-pointers are live locals, and
        // the set it points at lives as long as the description.
        let status = unsafe {
            sys::CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                description,
                index,
                &mut pointer,
                &mut size,
                &mut count,
                &mut header,
            )
        };

        (status == 0 && !pointer.is_null())
            // SAFETY: `pointer` is `size` readable bytes.
            .then(|| unsafe { std::slice::from_raw_parts(pointer, size) }.to_vec())
    };

    Some((set(0)?, set(1)?))
}

fn encode_error(reason: impl Into<String>) -> Error {
    Error::Encode {
        reason: reason.into(),
    }
}

fn check(status: sys::OSStatus, doing: &str) -> Result<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(encode_error(format!("{doing}: {}", describe(status))))
    }
}

/// An `OSStatus`, with its name where it is one VideoToolbox is known to return.
fn describe(status: sys::OSStatus) -> String {
    let meaning = match status {
        -12900 => "kVTPropertyNotSupportedErr — this encoder does not take that setting",
        -12902 => "kVTParameterErr",
        -12903 => "kVTInvalidSessionErr — the session was torn down, which sleep or a GPU switch can do",
        -12904 => "kVTAllocationFailedErr",
        -12908 => "kVTCouldNotFindVideoEncoderErr — nothing on this machine encodes this way",
        -12915 => "kVTVideoEncoderMalfunctionErr",
        -12916 => "kVTVideoEncoderNotAvailableNowErr — the media engine is busy",
        _ => return format!("OSStatus {status}"),
    };

    format!("OSStatus {status} ({meaning})")
}

fn to_cm_time(time: Duration) -> sys::CMTime {
    sys::CMTime {
        value: i64::try_from(time.as_nanos()).unwrap_or(i64::MAX),
        timescale: 1_000_000_000,
        flags: sys::kCMTimeFlags_Valid,
        epoch: 0,
    }
}

fn to_duration(time: sys::CMTime) -> Duration {
    if time.flags & sys::kCMTimeFlags_Valid == 0 || time.timescale <= 0 || time.value < 0 {
        return Duration::ZERO;
    }

    let nanoseconds = i128::from(time.value) * 1_000_000_000 / i128::from(time.timescale);

    Duration::from_nanos(u64::try_from(nanoseconds).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::PixelFormat;
    use crate::media::Rational;

    fn config(width: u32, height: u32) -> EncoderConfig {
        EncoderConfig::new(
            Encoding::H264,
            width,
            height,
            Rational::new(30, 1),
            ColorSpace::guess_for(1920, 1080),
        )
    }

    fn gray(width: u32, height: u32, shade: u8, index: u64) -> Frame {
        let mut data = vec![shade; (width * height) as usize];
        data.extend(std::iter::repeat_n(128, (width * height / 2) as usize));

        Frame::packed(
            width,
            height,
            PixelFormat::Nv12,
            ColorSpace::guess_for(1920, 1080),
            Duration::from_secs(index) / 30,
            data,
        )
        .unwrap()
    }

    #[test]
    fn only_h264_is_written_here() {
        let mut config = config(64, 64);
        config.encoding = Encoding::Av1;

        assert!(matches!(
            VideoToolboxEncoder::new(&config),
            Err(Error::NoEncoder { .. })
        ));
    }

    /// The spec, from the encoder's own output: an `avcC` for High 4.1, a
    /// keyframe exactly every two seconds, and packets in the order the
    /// pictures went in.
    #[test]
    fn the_stream_is_high_4_1_with_a_keyframe_every_two_seconds() {
        let mut encoder = VideoToolboxEncoder::new(&config(320, 240)).expect("VideoToolbox opens");
        let mut packets = Vec::new();

        for index in 0..150 {
            packets.extend(encoder.encode(&gray(320, 240, (index % 200) as u8, index)).unwrap());
        }
        packets.extend(encoder.finish().unwrap());

        assert_eq!(packets.len(), 150);

        let keyframes: Vec<usize> = packets
            .iter()
            .enumerate()
            .filter(|(_, packet)| packet.is_keyframe)
            .map(|(index, _)| index)
            .collect();
        assert_eq!(keyframes, [0, 60, 120], "two seconds at 30 fps, and nothing between");

        assert!(packets.windows(2).all(|pair| pair[0].pts < pair[1].pts));
        assert!(packets.iter().all(|packet| packet.pts == packet.dts), "no reordering");

        let record = encoder.config_record().expect("parameter sets after the first frame");
        let avc = AvcConfig::parse(&record).unwrap();
        let sps = crate::bitstream::SequenceParameterSet::parse(&avc.sequence_parameter_sets[0])
            .unwrap();

        assert_eq!(sps.profile_idc, 100, "High");
        assert_eq!(sps.level_idc, 41, "Level 4.1, declared");
        assert_eq!((sps.width, sps.height), (320, 240));
        assert_eq!((sps.bit_depth, sps.chroma_format_idc), (8, 1));
        assert_eq!(sps.matrix, Some(1), "tagged BT.709");
        assert_eq!(sps.full_range, Some(false));
        assert_eq!(sps.max_num_reorder_frames.unwrap_or(0), 0, "no B-frames");
    }

    #[test]
    fn software_encoding_on_a_mac_is_apples_own() {
        if !cfg!(target_os = "macos") {
            return;
        }

        let mut config = config(64, 64);
        config.hardware = Hardware::Off;

        let mut encoder = VideoToolboxEncoder::new(&config).expect("Apple's software encoder");
        assert!(!encoder.is_hardware());

        let mut packets = encoder.encode(&gray(64, 64, 90, 0)).unwrap();
        packets.extend(encoder.finish().unwrap());
        assert_eq!(packets.len(), 1);
        assert!(packets[0].is_keyframe);
    }

    #[test]
    fn a_picture_of_the_wrong_size_is_refused() {
        let mut encoder = VideoToolboxEncoder::new(&config(64, 64)).unwrap();

        assert!(encoder.encode(&gray(32, 32, 0, 0)).is_err());
    }
}
