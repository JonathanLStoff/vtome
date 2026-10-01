//! VideoToolbox: the decoder Apple ships, and the hardware behind it.
//!
//! Every call here is a plain C function in VideoToolbox, CoreMedia, CoreVideo,
//! or CoreFoundation, so this is `extern "C"` declarations and nothing more — no
//! Objective-C runtime and no crates, only the frameworks. The declarations
//! were checked against the macOS SDK headers; a mismatched one does not fail
//! to compile, it corrupts the stack.
//!
//! # One packet, start to finish
//!
//! 1. Once: the `avcC` record's SPS and PPS become a `CMVideoFormatDescription`,
//!    and a `VTDecompressionSession` opens on it asking for NV12 — what the
//!    hardware produces and what the renderer samples directly.
//! 2. Per packet: MP4 already stores H.264 as length-prefixed NAL units, which is
//!    exactly what VideoToolbox takes, so the bytes go into a `CMBlockBuffer`
//!    untouched and then into a `CMSampleBuffer` with their timestamps. Handing
//!    it Annex B instead is the classic mistake here: no error, and no picture.
//! 3. Decoding is synchronous. The output callback runs before
//!    `VTDecompressionSessionDecodeFrame` returns and copies both planes out of
//!    the `CVPixelBuffer` with the buffer's own strides.
//!
//! # Order
//!
//! Pictures come back in *decode* order — that is what VideoToolbox does with
//! temporal processing off, and it is off because on it may hold frames
//! indefinitely. With B-frames decode order is not display order, so a small
//! reorder queue holds pictures until nothing still to come can be shown
//! before them.
//!
//! # Not here yet
//!
//! HEVC and AV1: VideoToolbox decodes both, but their configuration records
//! (`hvcC`, `av1C`) are not wired to a format description yet, so they are
//! refused by name rather than attempted. And the planes are copied: the
//! zero-copy path hands the buffer's IOSurface to Metal. Both are in
//! `planning/TODO.md` §2.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::{c_int, c_void};
use std::ptr;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use crate::bitstream::AvcConfig;
use crate::color::{ColorSpace, Range};
use crate::decode::{Decoder, DecoderConfig};
use crate::error::{Error, Result};
use crate::frame::{Frame, PixelFormat, Plane};
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
    pub type CFStringRef = *const c_void;
    pub type CFNumberRef = *const c_void;
    pub type CFBooleanRef = *const c_void;
    pub type CFIndex = isize;
    pub type CMFormatDescriptionRef = *const c_void;
    pub type CMBlockBufferRef = *const c_void;
    pub type CMSampleBufferRef = *const c_void;
    pub type CVPixelBufferRef = *const c_void;
    pub type VTDecompressionSessionRef = *const c_void;

    /// `kCFAllocatorDefault`, which is a null pointer.
    pub const DEFAULT_ALLOCATOR: CFAllocatorRef = std::ptr::null();

    pub const kCFNumberSInt32Type: CFIndex = 3;
    pub const kCMTimeFlags_Valid: u32 = 1 << 0;
    pub const kCMBlockBufferAssureMemoryNowFlag: u32 = 1 << 0;
    pub const kCVPixelBufferLock_ReadOnly: u64 = 1 << 0;
    pub const kVTDecodeInfo_FrameDropped: u32 = 1 << 1;

    #[repr(C)]
    #[derive(Clone, Copy, Debug)]
    pub struct CMTime {
        pub value: i64,
        pub timescale: i32,
        pub flags: u32,
        pub epoch: i64,
    }

    /// `kCMTimeInvalid`: all zeros.
    pub const INVALID_TIME: CMTime = CMTime {
        value: 0,
        timescale: 0,
        flags: 0,
        epoch: 0,
    };

    #[repr(C)]
    pub struct CMSampleTimingInfo {
        pub duration: CMTime,
        pub presentation_time_stamp: CMTime,
        pub decode_time_stamp: CMTime,
    }

    pub type VTDecompressionOutputCallback = unsafe extern "C" fn(
        output_ref_con: *mut c_void,
        source_frame_ref_con: *mut c_void,
        status: OSStatus,
        info_flags: u32,
        image_buffer: CVPixelBufferRef,
        presentation_time_stamp: CMTime,
        presentation_duration: CMTime,
    );

    #[repr(C)]
    pub struct VTDecompressionOutputCallbackRecord {
        pub callback: VTDecompressionOutputCallback,
        pub ref_con: *mut c_void,
    }

    /// Only ever used by address, so its contents need not be spelled out.
    #[repr(C)]
    pub struct CFDictionaryCallBacks {
        _opaque: [u8; 0],
        _marker: PhantomData<(*mut u8, PhantomPinned)>,
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        pub static kCFTypeDictionaryKeyCallBacks: CFDictionaryCallBacks;
        pub static kCFTypeDictionaryValueCallBacks: CFDictionaryCallBacks;

        pub fn CFDictionaryCreate(
            allocator: CFAllocatorRef,
            keys: *const *const c_void,
            values: *const *const c_void,
            count: CFIndex,
            key_callbacks: *const CFDictionaryCallBacks,
            value_callbacks: *const CFDictionaryCallBacks,
        ) -> CFDictionaryRef;
        pub fn CFNumberCreate(
            allocator: CFAllocatorRef,
            number_type: CFIndex,
            value: *const c_void,
        ) -> CFNumberRef;
        pub fn CFBooleanGetValue(boolean: CFBooleanRef) -> u8;
        pub fn CFRelease(object: CFTypeRef);
    }

    #[link(name = "CoreMedia", kind = "framework")]
    extern "C" {
        pub fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
            allocator: CFAllocatorRef,
            parameter_set_count: usize,
            parameter_set_pointers: *const *const u8,
            parameter_set_sizes: *const usize,
            nal_unit_header_length: c_int,
            format_description_out: *mut CMFormatDescriptionRef,
        ) -> OSStatus;
        pub fn CMBlockBufferCreateWithMemoryBlock(
            structure_allocator: CFAllocatorRef,
            memory_block: *mut c_void,
            block_length: usize,
            block_allocator: CFAllocatorRef,
            custom_block_source: *const c_void,
            offset_to_data: usize,
            data_length: usize,
            flags: u32,
            block_buffer_out: *mut CMBlockBufferRef,
        ) -> OSStatus;
        pub fn CMBlockBufferReplaceDataBytes(
            source_bytes: *const c_void,
            destination_buffer: CMBlockBufferRef,
            offset_into_destination: usize,
            data_length: usize,
        ) -> OSStatus;
        pub fn CMSampleBufferCreateReady(
            allocator: CFAllocatorRef,
            data_buffer: CMBlockBufferRef,
            format_description: CMFormatDescriptionRef,
            num_samples: CFIndex,
            num_sample_timing_entries: CFIndex,
            sample_timing_array: *const CMSampleTimingInfo,
            num_sample_size_entries: CFIndex,
            sample_size_array: *const usize,
            sample_buffer_out: *mut CMSampleBufferRef,
        ) -> OSStatus;
    }

    #[link(name = "CoreVideo", kind = "framework")]
    extern "C" {
        pub static kCVPixelBufferPixelFormatTypeKey: CFStringRef;

        pub fn CVPixelBufferLockBaseAddress(buffer: CVPixelBufferRef, flags: u64) -> i32;
        pub fn CVPixelBufferUnlockBaseAddress(buffer: CVPixelBufferRef, flags: u64) -> i32;
        pub fn CVPixelBufferGetPixelFormatType(buffer: CVPixelBufferRef) -> u32;
        pub fn CVPixelBufferGetWidth(buffer: CVPixelBufferRef) -> usize;
        pub fn CVPixelBufferGetHeight(buffer: CVPixelBufferRef) -> usize;
        pub fn CVPixelBufferGetPlaneCount(buffer: CVPixelBufferRef) -> usize;
        pub fn CVPixelBufferGetBaseAddressOfPlane(buffer: CVPixelBufferRef, plane: usize)
            -> *mut c_void;
        pub fn CVPixelBufferGetBytesPerRowOfPlane(buffer: CVPixelBufferRef, plane: usize) -> usize;
        pub fn CVPixelBufferGetHeightOfPlane(buffer: CVPixelBufferRef, plane: usize) -> usize;
    }

    #[link(name = "VideoToolbox", kind = "framework")]
    extern "C" {
        // macOS 10.9, but iOS 17: declaring it on iOS would make every app
        // with an older deployment target fail to launch.
        #[cfg(target_os = "macos")]
        pub static kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder: CFStringRef;

        pub fn VTDecompressionSessionCreate(
            allocator: CFAllocatorRef,
            video_format_description: CMFormatDescriptionRef,
            video_decoder_specification: CFDictionaryRef,
            destination_image_buffer_attributes: CFDictionaryRef,
            output_callback: *const VTDecompressionOutputCallbackRecord,
            decompression_session_out: *mut VTDecompressionSessionRef,
        ) -> OSStatus;
        pub fn VTDecompressionSessionDecodeFrame(
            session: VTDecompressionSessionRef,
            sample_buffer: CMSampleBufferRef,
            decode_flags: u32,
            source_frame_ref_con: *mut c_void,
            info_flags_out: *mut u32,
        ) -> OSStatus;
        pub fn VTDecompressionSessionFinishDelayedFrames(session: VTDecompressionSessionRef)
            -> OSStatus;
        pub fn VTDecompressionSessionWaitForAsynchronousFrames(
            session: VTDecompressionSessionRef,
        ) -> OSStatus;
        pub fn VTDecompressionSessionInvalidate(session: VTDecompressionSessionRef);
        pub fn VTSessionCopyProperty(
            session: CFTypeRef,
            key: CFStringRef,
            allocator: CFAllocatorRef,
            value_out: *mut c_void,
        ) -> OSStatus;
    }
}

/// `kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange`: NV12, limited range.
const NV12_VIDEO_RANGE: u32 = u32::from_be_bytes(*b"420v");
/// `kCVPixelFormatType_420YpCbCr8BiPlanarFullRange`: NV12, full range.
const NV12_FULL_RANGE: u32 = u32::from_be_bytes(*b"420f");

/// How many pictures to hold when the container gives no decode times.
///
/// Matroska stores presentation time only, which leaves [`Reorder`]'s exact
/// rule with nothing to go on, so pictures wait until this many are queued.
/// That covers the B-frame depth every common encoder uses — x264's default is
/// two, Apple's one or two. Reading `max_num_reorder_frames` from the SPS would
/// make it exact (planning/TODO.md §2).
const REORDER_WINDOW: usize = 4;

/// One reference to a Core Foundation object, released when dropped.
///
/// Every `Create` and `Copy` call hands back an object the caller owns. Holding
/// each in one of these is what keeps an early `?` from leaking it.
struct Retained(sys::CFTypeRef);

impl Retained {
    /// Takes ownership of what a `Create` or `Copy` call just returned. Null is
    /// `None`, because a call can report success and still hand back nothing.
    ///
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

/// What the output callback writes into.
///
/// Boxed by the decoder so its address stays put when the decoder moves: the
/// session holds that address for its whole life.
struct Output {
    /// Stamped on every frame; the range is corrected per buffer.
    color: ColorSpace,
    /// Pictures finished since the last time the decoder looked.
    frames: Vec<Frame>,
    /// Why the last picture failed, if one did.
    failure: Option<String>,
}

/// H.264 through VideoToolbox.
///
/// Hardware-decoded wherever the machine has the silicon, which is every Mac
/// since 2011 — and [`is_hardware`](Decoder::is_hardware) asks the session
/// rather than assuming.
pub struct VideoToolboxDecoder {
    session: Retained,
    format: Retained,
    output: Box<Mutex<Output>>,
    reorder: Reorder,
    hardware: bool,
}

// SAFETY: Core Foundation objects and decompression sessions may be used from
// any thread, one at a time; `&mut self` on every call that touches the session
// guarantees the "one at a time", and the callback's state is behind a mutex.
unsafe impl Send for VideoToolboxDecoder {}

impl VideoToolboxDecoder {
    /// A decoder for the track `config` describes.
    ///
    /// # Errors
    ///
    /// [`Error::NoDecoder`] for anything but H.264, [`Error::Decode`] if the
    /// track has no usable `avcC` record or VideoToolbox will not open a session
    /// for it.
    pub fn new(config: &DecoderConfig) -> Result<Self> {
        if config.encoding != Encoding::H264 {
            let remedy = match config.encoding {
                // M3-class chips decode AV1 in hardware, and that path is
                // wanted; until it exists, AV1 is the software decoder's.
                Encoding::Av1 => "vtome drives VideoToolbox for H.264 only so far; AV1 in \
                                  hardware needs an av1C path to a format description \
                                  (planning/TODO.md §2), and until then AV1 is decode-av1's"
                    .to_string(),
                other => format!(
                    "vtome decodes H.264 and AV1 and nothing else; {other} is outside its scope"
                ),
            };

            return Err(Error::NoDecoder {
                encoding: config.encoding,
                remedy,
            });
        }

        if config.extra_data.is_empty() {
            return Err(decode_error(
                "the track carries no avcC record, and VideoToolbox cannot start without \
                 the SPS and PPS inside it",
            ));
        }

        let avc = AvcConfig::parse(&config.extra_data)?;
        let format = format_description(&avc)?;

        let output = Box::new(Mutex::new(Output {
            color: config.color,
            frames: Vec::new(),
            failure: None,
        }));

        let session = open_session(&format, &output)?;
        let hardware = uses_hardware(&session);

        Ok(VideoToolboxDecoder {
            session,
            format,
            output,
            reorder: Reorder::default(),
            hardware,
        })
    }

    /// Wraps one packet as a sample buffer the session will take.
    fn sample_buffer(&self, packet: &Packet) -> Result<Retained> {
        let length = packet.data.len();
        let mut block = ptr::null();

        // CoreMedia allocates the block and the bytes are copied in, so the
        // sample owns its data outright and the packet can go whenever it likes.
        // SAFETY: a null memory block with the assure-now flag asks CoreMedia
        // to allocate `length` bytes itself; `block` receives a +1 reference.
        let status = unsafe {
            sys::CMBlockBufferCreateWithMemoryBlock(
                sys::DEFAULT_ALLOCATOR,
                ptr::null_mut(),
                length,
                sys::DEFAULT_ALLOCATOR,
                ptr::null(),
                0,
                length,
                sys::kCMBlockBufferAssureMemoryNowFlag,
                &mut block,
            )
        };
        check(status, "allocating a block buffer")?;
        // SAFETY: a `Create` call's +1 reference.
        let block = unsafe { Retained::adopt(block) }
            .ok_or_else(|| decode_error("CoreMedia allocated no block buffer"))?;

        // SAFETY: the source is `length` readable bytes and the block was
        // allocated at exactly that length.
        let status = unsafe {
            sys::CMBlockBufferReplaceDataBytes(packet.data.as_ptr().cast(), block.get(), 0, length)
        };
        check(status, "copying a packet into its block buffer")?;

        let timing = sys::CMSampleTimingInfo {
            duration: sys::INVALID_TIME,
            presentation_time_stamp: to_cm_time(packet.pts),
            decode_time_stamp: to_cm_time(packet.dts),
        };

        let mut sample = ptr::null();

        // SAFETY: one sample, one timing entry, one size — each pointer names a
        // single live value, and the sample retains the block itself.
        let status = unsafe {
            sys::CMSampleBufferCreateReady(
                sys::DEFAULT_ALLOCATOR,
                block.get(),
                self.format.get(),
                1,
                1,
                &timing,
                1,
                &length,
                &mut sample,
            )
        };
        check(status, "wrapping a packet as a sample buffer")?;

        // SAFETY: a `Create` call's +1 reference.
        unsafe { Retained::adopt(sample) }
            .ok_or_else(|| decode_error("CoreMedia made no sample buffer"))
    }

    /// Moves whatever the callback produced into the reorder queue, and turns
    /// either failure — the call's or the callback's — into an error.
    fn collect(&mut self, status: sys::OSStatus) -> Result<()> {
        let (frames, failure) = {
            let mut output = self.output.lock().unwrap_or_else(PoisonError::into_inner);
            (std::mem::take(&mut output.frames), output.failure.take())
        };

        for frame in frames {
            self.reorder.push(frame);
        }

        check(status, "decoding a packet")?;

        match failure {
            Some(reason) => Err(decode_error(format!("decoding a packet: {reason}"))),
            None => Ok(()),
        }
    }
}

impl Decoder for VideoToolboxDecoder {
    fn encoding(&self) -> Encoding {
        Encoding::H264
    }

    fn decode(&mut self, packet: &Packet) -> Result<Option<Frame>> {
        // Zero bytes means "nothing this interval" in some containers, and
        // VideoToolbox would call it bad data.
        if !packet.is_empty() {
            let sample = self.sample_buffer(packet)?;
            let mut info = 0_u32;

            // Flags 0: synchronous and no temporal processing, so every picture
            // this packet completes reaches `on_decoded` before this returns.
            // SAFETY: both references are live for the call.
            let status = unsafe {
                sys::VTDecompressionSessionDecodeFrame(
                    self.session.get(),
                    sample.get(),
                    0,
                    ptr::null_mut(),
                    &mut info,
                )
            };

            self.collect(status)?;
        }

        self.reorder.fed(packet);
        self.reorder.release();

        Ok(self.reorder.next())
    }

    fn flush(&mut self) -> Result<Vec<Frame>> {
        // SAFETY: the session is live until `drop`.
        let status = unsafe {
            let finished = sys::VTDecompressionSessionFinishDelayedFrames(self.session.get());
            let waited = sys::VTDecompressionSessionWaitForAsynchronousFrames(self.session.get());

            if finished != 0 {
                finished
            } else {
                waited
            }
        };

        self.collect(status)?;

        Ok(self.reorder.drain())
    }

    fn reset(&mut self) -> Result<()> {
        // Anything still in flight belongs to the old position; wait for it and
        // throw it away. The next packet after a seek is a keyframe, which
        // restarts the reference chain inside the decoder on its own.
        // SAFETY: the session is live until `drop`.
        unsafe {
            sys::VTDecompressionSessionFinishDelayedFrames(self.session.get());
            sys::VTDecompressionSessionWaitForAsynchronousFrames(self.session.get());
        }

        let mut output = self.output.lock().unwrap_or_else(PoisonError::into_inner);
        output.frames.clear();
        output.failure = None;
        drop(output);

        self.reorder = Reorder::default();

        Ok(())
    }

    fn is_hardware(&self) -> bool {
        self.hardware
    }
}

impl Drop for VideoToolboxDecoder {
    fn drop(&mut self) {
        // After this no callback can arrive, so `output` may go with the other
        // fields. Releasing the session without it is legal but leaves the
        // teardown to whenever the last internal reference goes.
        // SAFETY: the session is live, and this is the last use of it.
        unsafe { sys::VTDecompressionSessionInvalidate(self.session.get()) }
    }
}

/// The format description for an `avcC` record's parameter sets.
fn format_description(avc: &AvcConfig) -> Result<Retained> {
    if avc.sequence_parameter_sets.is_empty() || avc.picture_parameter_sets.is_empty() {
        return Err(decode_error(format!(
            "the avcC record holds {} SPS and {} PPS; VideoToolbox needs at least one of each",
            avc.sequence_parameter_sets.len(),
            avc.picture_parameter_sets.len()
        )));
    }

    let sets: Vec<&[u8]> = avc
        .sequence_parameter_sets
        .iter()
        .chain(&avc.picture_parameter_sets)
        .map(Vec::as_slice)
        .collect();

    let pointers: Vec<*const u8> = sets.iter().map(|set| set.as_ptr()).collect();
    let sizes: Vec<usize> = sets.iter().map(|set| set.len()).collect();

    let mut format = ptr::null();

    // SAFETY: `pointers` and `sizes` describe `sets`, which outlive the call,
    // and CoreMedia copies what it keeps. `format` receives a +1 reference.
    let status = unsafe {
        sys::CMVideoFormatDescriptionCreateFromH264ParameterSets(
            sys::DEFAULT_ALLOCATOR,
            sets.len(),
            pointers.as_ptr(),
            sizes.as_ptr(),
            avc.nal_length_size as c_int,
            &mut format,
        )
    };
    check(status, "reading the SPS and PPS")?;

    // SAFETY: a `Create` call's +1 reference.
    unsafe { Retained::adopt(format) }
        .ok_or_else(|| decode_error("CoreMedia made no format description from the SPS and PPS"))
}

/// A session for `format` that decodes to NV12 and reports into `output`.
fn open_session(format: &Retained, output: &Mutex<Output>) -> Result<Retained> {
    let attributes = nv12_attributes()?;

    // The record is copied by the call; the pointer inside it is what has to
    // stay valid, which is why `output` is boxed.
    let callback = sys::VTDecompressionOutputCallbackRecord {
        callback: on_decoded,
        ref_con: ptr::from_ref(output).cast_mut().cast(),
    };

    let mut session = ptr::null();

    // SAFETY: every reference passed is live for the call; no decoder
    // specification means "hardware if there is any", the default since
    // macOS 10.15. `session` receives a +1 reference.
    let status = unsafe {
        sys::VTDecompressionSessionCreate(
            sys::DEFAULT_ALLOCATOR,
            format.get(),
            ptr::null(),
            attributes.get(),
            &callback,
            &mut session,
        )
    };
    check(status, "opening a decompression session")?;

    // SAFETY: a `Create` call's +1 reference.
    unsafe { Retained::adopt(session) }
        .ok_or_else(|| decode_error("VideoToolbox opened no session"))
}

/// `{ kCVPixelBufferPixelFormatTypeKey: '420v' }`.
fn nv12_attributes() -> Result<Retained> {
    let format = NV12_VIDEO_RANGE as i32;

    // SAFETY: a 32-bit integer read as `kCFNumberSInt32Type`.
    let number = unsafe {
        Retained::adopt(sys::CFNumberCreate(
            sys::DEFAULT_ALLOCATOR,
            sys::kCFNumberSInt32Type,
            ptr::from_ref(&format).cast(),
        ))
    }
    .ok_or_else(|| decode_error("CoreFoundation made no number"))?;

    // SAFETY: an immutable framework constant.
    let keys = [unsafe { sys::kCVPixelBufferPixelFormatTypeKey }];
    let values = [number.get()];

    // SAFETY: one key and one value, both live; the CFType callbacks retain
    // them, so `number` can be released when this returns.
    let dictionary = unsafe {
        sys::CFDictionaryCreate(
            sys::DEFAULT_ALLOCATOR,
            keys.as_ptr(),
            values.as_ptr(),
            1,
            ptr::addr_of!(sys::kCFTypeDictionaryKeyCallBacks),
            ptr::addr_of!(sys::kCFTypeDictionaryValueCallBacks),
        )
    };

    // SAFETY: a `Create` call's +1 reference.
    unsafe { Retained::adopt(dictionary) }
        .ok_or_else(|| decode_error("CoreFoundation made no dictionary"))
}

/// Whether the session landed on the hardware decoder, by asking it.
#[cfg(target_os = "macos")]
fn uses_hardware(session: &Retained) -> bool {
    let mut value: sys::CFBooleanRef = ptr::null();

    // SAFETY: a property this session type defines, copied out as a +1 CFBoolean.
    let status = unsafe {
        sys::VTSessionCopyProperty(
            session.get(),
            sys::kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder,
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

/// iOS, tvOS, and visionOS decode H.264 in hardware on every device they run
/// on, and the property that would confirm it only exists from iOS 17.
#[cfg(not(target_os = "macos"))]
fn uses_hardware(_session: &Retained) -> bool {
    true
}

/// Called by VideoToolbox once per decoded picture — during
/// `VTDecompressionSessionDecodeFrame`, since decoding is synchronous.
unsafe extern "C" fn on_decoded(
    output: *mut c_void,
    _source_frame: *mut c_void,
    status: sys::OSStatus,
    info_flags: u32,
    image: sys::CVPixelBufferRef,
    pts: sys::CMTime,
    _duration: sys::CMTime,
) {
    // A panic must not unwind into VideoToolbox's stack frames.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: `output` is the boxed mutex the decoder registered, and the
        // decoder invalidates the session before that box is freed.
        let output = unsafe { &*output.cast_const().cast::<Mutex<Output>>() };
        let mut output = output.lock().unwrap_or_else(PoisonError::into_inner);

        if status != 0 {
            output.failure = Some(describe(status));
            return;
        }

        // A picture the decoder chose not to produce — a leading frame after a
        // seek, say — is not a failure.
        if image.is_null() || info_flags & sys::kVTDecodeInfo_FrameDropped != 0 {
            return;
        }

        let color = output.color;

        // SAFETY: VideoToolbox guarantees `image` for the duration of the call.
        match unsafe { copy_out(image, to_duration(pts), color) } {
            Ok(frame) => output.frames.push(frame),
            Err(error) => output.failure = Some(error.to_string()),
        }
    }));
}

/// Copies an NV12 `CVPixelBuffer` into a [`Frame`], strides and all.
///
/// # Safety
///
/// `image` must be a live `CVPixelBuffer` for the duration of the call.
unsafe fn copy_out(image: sys::CVPixelBufferRef, pts: Duration, color: ColorSpace) -> Result<Frame> {
    // Asked for '420v'; '420f' is accepted too, and the range follows whichever
    // arrived rather than whatever the file claimed.
    let range = match sys::CVPixelBufferGetPixelFormatType(image) {
        NV12_VIDEO_RANGE => Range::Limited,
        NV12_FULL_RANGE => Range::Full,
        other => {
            return Err(decode_error(format!(
                "VideoToolbox returned pixel format '{}' rather than the NV12 asked for",
                String::from_utf8_lossy(&other.to_be_bytes())
            )))
        }
    };

    if sys::CVPixelBufferGetPlaneCount(image) != 2 {
        return Err(decode_error("an NV12 buffer arrived without two planes"));
    }

    if sys::CVPixelBufferLockBaseAddress(image, sys::kCVPixelBufferLock_ReadOnly) != 0 {
        return Err(decode_error("the decoded picture could not be locked for reading"));
    }

    /// Unlocks on every way out, including the `?`s below.
    struct Unlock(sys::CVPixelBufferRef);

    impl Drop for Unlock {
        fn drop(&mut self) {
            // SAFETY: locked above with the same flags.
            unsafe {
                sys::CVPixelBufferUnlockBaseAddress(self.0, sys::kCVPixelBufferLock_ReadOnly);
            }
        }
    }

    let _unlock = Unlock(image);

    let too_big = || decode_error("the decoded picture is larger than a u32 can describe");
    let width = u32::try_from(sys::CVPixelBufferGetWidth(image)).map_err(|_| too_big())?;
    let height = u32::try_from(sys::CVPixelBufferGetHeight(image)).map_err(|_| too_big())?;

    let layout: Vec<(*const u8, usize, usize)> = (0..2)
        .map(|plane| {
            (
                sys::CVPixelBufferGetBaseAddressOfPlane(image, plane)
                    .cast_const()
                    .cast::<u8>(),
                sys::CVPixelBufferGetBytesPerRowOfPlane(image, plane),
                sys::CVPixelBufferGetHeightOfPlane(image, plane),
            )
        })
        .collect();

    let total = layout
        .iter()
        .try_fold(0_usize, |sum, (_, stride, rows)| {
            stride.checked_mul(*rows).and_then(|size| sum.checked_add(size))
        })
        .ok_or_else(too_big)?;

    let mut data = Vec::with_capacity(total);
    let mut planes = Vec::with_capacity(2);

    for (base, stride, rows) in layout {
        if base.is_null() {
            return Err(decode_error("a locked plane had no address"));
        }

        planes.push(Plane {
            offset: data.len(),
            stride,
        });

        // SAFETY: a locked plane is `stride × rows` readable bytes, and the
        // lock is held until `_unlock` drops.
        data.extend_from_slice(std::slice::from_raw_parts(base, stride * rows));
    }

    // `with_planes` checks the layout against the buffer, so a plane
    // VideoToolbox sized differently from NV12's rules is refused here rather
    // than read past on the GPU.
    Frame::with_planes(
        width,
        height,
        PixelFormat::Nv12,
        ColorSpace { range, ..color },
        pts,
        planes,
        data,
    )
}

/// Pictures waiting to be shown in order.
///
/// A picture may leave once nothing still to be decoded can be shown before
/// it. Where packets carry real decode times — MP4 and MOV — that moment is
/// exact: every sample is shown no earlier than it is decoded, so once the
/// packet decoding at `t` has gone in, everything still to come is shown after
/// `t`, and everything waiting with a PTS at or before `t` can go. Files
/// written with negative composition offsets show some pictures *before* their
/// decode time; the largest such lead seen is subtracted to allow for it.
///
/// Where every decode time equals its PTS — Matroska, which stores only the
/// latter — that rule would release everything at once, so a fixed
/// [`REORDER_WINDOW`] holds instead.
#[derive(Default)]
struct Reorder {
    /// Keyed on PTS, then on arrival so that two equal stamps both survive.
    waiting: BTreeMap<(Duration, u64), Frame>,
    ready: VecDeque<Frame>,
    arrivals: u64,
    decode_times_are_real: bool,
    decoded_to: Duration,
    lead: Duration,
}

impl Reorder {
    /// Notes the timing of a packet that has just gone into the decoder.
    fn fed(&mut self, packet: &Packet) {
        if packet.dts != packet.pts {
            self.decode_times_are_real = true;
        }

        self.lead = self.lead.max(packet.dts.saturating_sub(packet.pts));
        self.decoded_to = packet.dts;
    }

    fn push(&mut self, frame: Frame) {
        self.waiting.insert((frame.pts(), self.arrivals), frame);
        self.arrivals += 1;
    }

    /// Moves every picture that can no longer be overtaken to `ready`.
    fn release(&mut self) {
        loop {
            let waiting = self.waiting.len();

            let Some(entry) = self.waiting.first_entry() else {
                break;
            };

            let (pts, _) = *entry.key();

            let due = if self.decode_times_are_real {
                pts + self.lead <= self.decoded_to
            } else {
                waiting > REORDER_WINDOW
            };

            if !due {
                break;
            }

            self.ready.push_back(entry.remove());
        }
    }

    fn next(&mut self) -> Option<Frame> {
        self.ready.pop_front()
    }

    /// Everything, in order, at the end of the stream.
    fn drain(&mut self) -> Vec<Frame> {
        let mut frames: Vec<Frame> = self.ready.drain(..).collect();
        frames.extend(std::mem::take(&mut self.waiting).into_values());
        frames
    }
}

fn decode_error(reason: impl Into<String>) -> Error {
    Error::Decode {
        encoding: Encoding::H264,
        reason: reason.into(),
    }
}

/// `Ok` for `noErr`, otherwise an error saying what was being attempted.
fn check(status: sys::OSStatus, doing: &str) -> Result<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(decode_error(format!("{doing}: {}", describe(status))))
    }
}

/// An `OSStatus`, with its name where it is one VideoToolbox is known to return.
fn describe(status: sys::OSStatus) -> String {
    let meaning = match status {
        -12902 => "kVTParameterErr",
        -12903 => "kVTInvalidSessionErr — the session was torn down, which sleep or a GPU switch can do",
        -12904 => "kVTAllocationFailedErr",
        -12906 => "kVTCouldNotFindVideoDecoderErr — nothing on this machine decodes this stream",
        -12909 => "kVTVideoDecoderBadDataErr — the bytes are corrupt, or not what the avcC record describes",
        -12910 => "kVTVideoDecoderUnsupportedDataFormatErr — a profile or feature this decoder does not take",
        -12911 => "kVTVideoDecoderMalfunctionErr",
        -12913 => "kVTVideoDecoderNotAvailableNowErr — the hardware decoder is busy or unavailable",
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

    fn frame_at(milliseconds: u64) -> Frame {
        Frame::packed(
            2,
            2,
            PixelFormat::Nv12,
            ColorSpace::default(),
            Duration::from_millis(milliseconds),
            vec![0; 6],
        )
        .unwrap()
    }

    fn packet(pts: u64, dts: u64) -> Packet {
        Packet {
            track_id: 1,
            data: vec![1],
            pts: Duration::from_millis(pts),
            dts: Duration::from_millis(dts),
            is_keyframe: false,
        }
    }

    fn order(reorder: &mut Reorder, arrivals: &[(u64, u64)]) -> Vec<u64> {
        let mut shown = Vec::new();

        for &(pts, dts) in arrivals {
            reorder.push(frame_at(pts));
            reorder.fed(&packet(pts, dts));
            reorder.release();

            while let Some(frame) = reorder.next() {
                shown.push(frame.pts().as_millis() as u64);
            }
        }

        shown.extend(reorder.drain().iter().map(|frame| frame.pts().as_millis() as u64));
        shown
    }

    /// x264's default: two B-frames, decode order I P B B P B B, and the
    /// composition offsets an MP4 muxer writes for it.
    #[test]
    fn mp4_decode_times_put_b_frames_back_in_display_order() {
        let arrivals = [(2, 0), (5, 1), (3, 2), (4, 3), (8, 4), (6, 5), (7, 6)];

        assert_eq!(order(&mut Reorder::default(), &arrivals), [2, 3, 4, 5, 6, 7, 8]);
    }

    /// With real decode times a picture is released as soon as it is safe,
    /// not held for a window.
    #[test]
    fn mp4_decode_times_release_without_waiting_for_a_window() {
        let mut reorder = Reorder::default();

        reorder.push(frame_at(2));
        reorder.fed(&packet(2, 0));
        reorder.release();
        assert!(reorder.next().is_none(), "a B-frame could still come before it");

        reorder.push(frame_at(5));
        reorder.fed(&packet(5, 1));
        reorder.push(frame_at(3));
        reorder.fed(&packet(3, 2));
        reorder.release();

        assert_eq!(reorder.next().map(|frame| frame.pts()), Some(Duration::from_millis(2)));
        assert!(reorder.next().is_none());
    }

    /// Version-1 `ctts`: the first frame at zero, B-frames shown before their
    /// decode time.
    #[test]
    fn negative_composition_offsets_still_come_out_in_order() {
        let arrivals = [(2, 2), (5, 3), (3, 4), (4, 5), (8, 6), (6, 7), (7, 8)];

        assert_eq!(order(&mut Reorder::default(), &arrivals), [2, 3, 4, 5, 6, 7, 8]);
    }

    /// Matroska: decode time equal to PTS, so only the window keeps order.
    #[test]
    fn without_decode_times_the_window_keeps_order() {
        let arrivals = [(0, 0), (3, 3), (1, 1), (2, 2), (6, 6), (4, 4), (5, 5)];

        assert_eq!(order(&mut Reorder::default(), &arrivals), [0, 1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn times_survive_the_trip_through_cm_time() {
        let time = Duration::from_nanos(41_708_333);

        assert_eq!(to_duration(to_cm_time(time)), time);
        assert_eq!(to_duration(sys::INVALID_TIME), Duration::ZERO);
    }

    #[test]
    fn only_h264_is_attempted_and_the_rest_are_refused_by_name() {
        let config = |encoding| DecoderConfig {
            encoding,
            width: 64,
            height: 64,
            bit_depth: 8,
            color: ColorSpace::default(),
            extra_data: vec![1, 2, 3],
        };

        let Err(Error::NoDecoder { remedy, .. }) = VideoToolboxDecoder::new(&config(Encoding::H265))
        else {
            panic!("HEVC is outside vtome's scope");
        };
        assert!(remedy.contains("scope"), "{remedy}");

        let Err(Error::NoDecoder { remedy, .. }) = VideoToolboxDecoder::new(&config(Encoding::Av1))
        else {
            panic!("AV1 through VideoToolbox is not wired up yet");
        };
        assert!(remedy.contains("decode-av1"), "{remedy}");
    }

    #[test]
    fn a_track_without_parameter_sets_is_refused_before_touching_videotoolbox() {
        let config = DecoderConfig {
            encoding: Encoding::H264,
            width: 64,
            height: 64,
            bit_depth: 8,
            color: ColorSpace::default(),
            extra_data: Vec::new(),
        };

        let Err(Error::Decode { reason, .. }) = VideoToolboxDecoder::new(&config) else {
            panic!("no avcC, no decoder");
        };

        assert!(reason.contains("avcC"), "{reason}");
    }

    #[test]
    fn named_statuses_say_what_they_mean() {
        assert!(describe(-12909).contains("BadData"));
        assert_eq!(describe(-1), "OSStatus -1");
    }
}
