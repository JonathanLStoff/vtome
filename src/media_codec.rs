//! MediaCodec: the H.264 encoder and decoder Android ships.
//!
//! The Android counterpart of VideoToolbox and Media Foundation: the codec is
//! the device's, so the H.264 licence is the device maker's. MediaCodec's NDK
//! API is plain C in `libmediandk`, so — as for VideoToolbox — it is declared
//! here by hand and pulls in no crate.
//!
//! Everything declared directly is in the NDK from API 21. The few calls that
//! came later — asking a codec its name (28), forcing a keyframe (26), reading
//! the input layout (28) — are looked up at run time, so an older device runs
//! without them rather than failing to load at all.
//!
//! # The encoder is set to §15
//!
//! High profile at Level 4.1, NV12 in, no B-frames, a two-second I-frame
//! interval — with every two-second frame also asked for as a sync frame where
//! the device takes `request-sync` — and VBR at the quality's bitrate.
//! MediaCodec's Annex B output is taken apart into MP4's shape by
//! [`AccessUnit`](crate::bitstream::AccessUnit).
//!
//! Type-checked for Android on every change; run on a device by hand.

#![allow(non_camel_case_types)]

use std::collections::VecDeque;
use std::ffi::{c_char, c_void, CStr};
use std::time::Duration;

use crate::bitstream::{length_prefixed_to_annex_b, AccessUnit, AvcConfig};
use crate::color::ColorSpace;
use crate::decode::{Decoder, DecoderConfig, Hardware};
use crate::encode::{Backend, Encoder, EncoderConfig};
use crate::error::{Error, Result};
use crate::frame::{Frame, PixelFormat, Plane};
use crate::identify::Encoding;
use crate::media::Packet;

mod sys {
    use std::ffi::{c_char, c_void};

    pub type AMediaCodec = c_void;
    pub type AMediaFormat = c_void;
    pub type media_status_t = i32;

    #[repr(C)]
    #[derive(Default)]
    pub struct AMediaCodecBufferInfo {
        pub offset: i32,
        pub size: i32,
        pub presentation_time_us: i64,
        pub flags: u32,
    }

    pub const CONFIGURE_FLAG_ENCODE: u32 = 1;
    pub const BUFFER_FLAG_KEY_FRAME: u32 = 1;
    pub const BUFFER_FLAG_CODEC_CONFIG: u32 = 2;
    pub const BUFFER_FLAG_END_OF_STREAM: u32 = 4;
    pub const INFO_TRY_AGAIN_LATER: isize = -1;
    pub const INFO_OUTPUT_FORMAT_CHANGED: isize = -2;

    /// `COLOR_FormatYUV420SemiPlanar`: NV12.
    pub const COLOR_NV12: i32 = 21;
    /// `COLOR_FormatYUV420Planar`: I420.
    pub const COLOR_I420: i32 = 19;
    /// `AVCProfileHigh`.
    pub const PROFILE_HIGH: i32 = 8;
    /// `AVCLevel41`.
    pub const LEVEL_4_1: i32 = 0x1000;
    /// `BITRATE_MODE_VBR`.
    pub const BITRATE_VBR: i32 = 1;

    #[link(name = "mediandk")]
    extern "C" {
        pub fn AMediaCodec_createEncoderByType(mime: *const c_char) -> *mut AMediaCodec;
        pub fn AMediaCodec_createDecoderByType(mime: *const c_char) -> *mut AMediaCodec;
        pub fn AMediaCodec_createCodecByName(name: *const c_char) -> *mut AMediaCodec;
        pub fn AMediaCodec_configure(
            codec: *mut AMediaCodec,
            format: *const AMediaFormat,
            surface: *mut c_void,
            crypto: *mut c_void,
            flags: u32,
        ) -> media_status_t;
        pub fn AMediaCodec_start(codec: *mut AMediaCodec) -> media_status_t;
        pub fn AMediaCodec_stop(codec: *mut AMediaCodec) -> media_status_t;
        pub fn AMediaCodec_delete(codec: *mut AMediaCodec) -> media_status_t;
        pub fn AMediaCodec_flush(codec: *mut AMediaCodec) -> media_status_t;
        pub fn AMediaCodec_dequeueInputBuffer(codec: *mut AMediaCodec, timeout_us: i64) -> isize;
        pub fn AMediaCodec_getInputBuffer(codec: *mut AMediaCodec, index: usize, size: *mut usize) -> *mut u8;
        pub fn AMediaCodec_queueInputBuffer(
            codec: *mut AMediaCodec,
            index: usize,
            offset: isize,
            size: usize,
            time_us: u64,
            flags: u32,
        ) -> media_status_t;
        pub fn AMediaCodec_dequeueOutputBuffer(
            codec: *mut AMediaCodec,
            info: *mut AMediaCodecBufferInfo,
            timeout_us: i64,
        ) -> isize;
        pub fn AMediaCodec_getOutputBuffer(codec: *mut AMediaCodec, index: usize, size: *mut usize) -> *mut u8;
        pub fn AMediaCodec_releaseOutputBuffer(codec: *mut AMediaCodec, index: usize, render: bool) -> media_status_t;
        pub fn AMediaCodec_getOutputFormat(codec: *mut AMediaCodec) -> *mut AMediaFormat;

        pub fn AMediaFormat_new() -> *mut AMediaFormat;
        pub fn AMediaFormat_delete(format: *mut AMediaFormat) -> media_status_t;
        pub fn AMediaFormat_setInt32(format: *mut AMediaFormat, name: *const c_char, value: i32);
        pub fn AMediaFormat_setString(format: *mut AMediaFormat, name: *const c_char, value: *const c_char);
        pub fn AMediaFormat_setBuffer(format: *mut AMediaFormat, name: *const c_char, data: *const c_void, size: usize);
        pub fn AMediaFormat_getInt32(format: *mut AMediaFormat, name: *const c_char, out: *mut i32) -> bool;
    }

    #[link(name = "dl")]
    extern "C" {
        pub fn dlopen(name: *const c_char, flags: i32) -> *mut c_void;
        pub fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    }

    pub const RTLD_NOW: i32 = 2;
}

/// A MediaCodec call that only exists on newer devices, looked up by name.
fn later<T: Copy>(symbol: &CStr) -> Option<T> {
    // SAFETY: `libmediandk.so` is already loaded by the link above; looking a
    // symbol up has no side effects, and `T` is the exact function pointer
    // type the NDK headers give it at each call site.
    unsafe {
        let library = sys::dlopen(c"libmediandk.so".as_ptr(), sys::RTLD_NOW);
        if library.is_null() {
            return None;
        }
        let found = sys::dlsym(library, symbol.as_ptr());
        (!found.is_null()).then(|| std::mem::transmute_copy::<*mut c_void, T>(&found))
    }
}

/// An `AMediaFormat`, deleted when dropped.
struct Format(*mut sys::AMediaFormat);

impl Format {
    fn new() -> Result<Self> {
        // SAFETY: a fresh format, owned here.
        let format = unsafe { sys::AMediaFormat_new() };
        (!format.is_null())
            .then_some(Format(format))
            .ok_or_else(|| Error::unsupported("MediaCodec made no format"))
    }

    fn int(&self, key: &CStr, value: i32) {
        // SAFETY: a live format and a NUL-terminated key.
        unsafe { sys::AMediaFormat_setInt32(self.0, key.as_ptr(), value) }
    }

    fn get(&self, key: &CStr) -> Option<i32> {
        let mut value = 0;
        // SAFETY: a live format, a key, and a live out-pointer.
        unsafe { sys::AMediaFormat_getInt32(self.0, key.as_ptr(), &mut value) }.then_some(value)
    }
}

impl Drop for Format {
    fn drop(&mut self) {
        // SAFETY: owned outright.
        unsafe { sys::AMediaFormat_delete(self.0) };
    }
}

/// A started codec, stopped and deleted when dropped.
struct Codec {
    codec: *mut sys::AMediaCodec,
    hardware: bool,
}

// SAFETY: MediaCodec's synchronous NDK API may be called from any thread, one
// call at a time, which `&mut self` guarantees.
unsafe impl Send for Codec {}

impl Codec {
    /// The device's codec for H.264 — its own, chosen by type, or Google's
    /// software one, chosen by name — as `hardware` allows.
    fn open(encoder: bool, hardware: Hardware) -> Result<Self> {
        let mime = c"video/avc";

        let software: &[&CStr] = if encoder {
            &[c"c2.android.avc.encoder", c"OMX.google.h264.encoder"]
        } else {
            &[c"c2.android.avc.decoder", c"OMX.google.h264.decoder"]
        };

        // SAFETY: NUL-terminated names; a null result is a refusal.
        let codec = unsafe {
            match hardware {
                Hardware::Off => software
                    .iter()
                    .map(|name| sys::AMediaCodec_createCodecByName(name.as_ptr()))
                    .find(|codec| !codec.is_null())
                    .unwrap_or(std::ptr::null_mut()),
                _ if encoder => sys::AMediaCodec_createEncoderByType(mime.as_ptr()),
                _ => sys::AMediaCodec_createDecoderByType(mime.as_ptr()),
            }
        };

        if codec.is_null() {
            return Err(Error::unsupported("this device has no H.264 codec MediaCodec will open"));
        }

        let name = name_of(codec);
        let hardware_codec = name.as_deref().is_none_or(|name| !is_software(name));

        let codec = Codec {
            codec,
            hardware: hardware_codec,
        };

        if hardware == Hardware::Require && !codec.hardware {
            return Err(Error::unsupported(
                "hardware was required, and this device's H.264 codec is software",
            ));
        }

        Ok(codec)
    }

    fn configure(&self, format: &Format, flags: u32) -> Result<()> {
        // SAFETY: a live codec and format; no surface, no crypto.
        let status = unsafe {
            sys::AMediaCodec_configure(self.codec, format.0, std::ptr::null_mut(), std::ptr::null_mut(), flags)
        };
        check(status, "configuring")?;
        // SAFETY: a configured codec.
        check(unsafe { sys::AMediaCodec_start(self.codec) }, "starting")
    }

    /// Copies `data` into the next input buffer and queues it — `false`, with
    /// nothing queued, if no buffer came free within 10 ms. The caller takes
    /// output, which is what frees input, and tries again.
    fn try_queue(&self, data: &[u8], time: Duration, flags: u32) -> Result<bool> {
        {
            // SAFETY: a started codec.
            let index = unsafe { sys::AMediaCodec_dequeueInputBuffer(self.codec, 10_000) };

            if index < 0 {
                return Ok(false);
            }

            let mut size = 0;
            // SAFETY: an index the codec just handed out.
            let buffer = unsafe { sys::AMediaCodec_getInputBuffer(self.codec, index as usize, &mut size) };
            if buffer.is_null() || size < data.len() {
                return Err(Error::Encode {
                    reason: format!("a MediaCodec input buffer of {size} bytes for {}", data.len()),
                });
            }

            // SAFETY: `size` writable bytes at `buffer`, and `data` fits.
            unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), buffer, data.len()) };

            let micros = u64::try_from(time.as_micros()).unwrap_or(u64::MAX);
            // SAFETY: the index handed out above, filled to `data.len()`.
            let status = unsafe {
                sys::AMediaCodec_queueInputBuffer(self.codec, index as usize, 0, data.len(), micros, flags)
            };
            check(status, "queueing input").map(|()| true)
        }
    }

    /// One finished output buffer — its bytes, info — or `None` after
    /// `timeout` with nothing ready. A format change is reported as an empty
    /// buffer with no flags, for the caller to re-read the format.
    fn dequeue(&self, timeout: Duration) -> Option<(Vec<u8>, sys::AMediaCodecBufferInfo)> {
        let mut info = sys::AMediaCodecBufferInfo::default();
        let micros = i64::try_from(timeout.as_micros()).unwrap_or(i64::MAX);

        // SAFETY: a started codec and a live out-pointer.
        let index = unsafe { sys::AMediaCodec_dequeueOutputBuffer(self.codec, &mut info, micros) };

        if index == sys::INFO_OUTPUT_FORMAT_CHANGED {
            return Some((Vec::new(), sys::AMediaCodecBufferInfo::default()));
        }
        if index < 0 {
            // Try again later, or buffers changed: nothing to hand on.
            return (index != sys::INFO_TRY_AGAIN_LATER).then(|| (Vec::new(), sys::AMediaCodecBufferInfo::default()));
        }

        let mut size = 0;
        // SAFETY: an index the codec just handed out, released below.
        let bytes = unsafe {
            let buffer = sys::AMediaCodec_getOutputBuffer(self.codec, index as usize, &mut size);
            let start = info.offset.max(0) as usize;
            let length = (info.size.max(0) as usize).min(size.saturating_sub(start));
            let bytes = if buffer.is_null() {
                Vec::new()
            } else {
                std::slice::from_raw_parts(buffer.add(start), length).to_vec()
            };
            sys::AMediaCodec_releaseOutputBuffer(self.codec, index as usize, false);
            bytes
        };

        Some((bytes, info))
    }

    fn output_format(&self) -> Option<Format> {
        // SAFETY: a started codec; the format returned is the caller's.
        let format = unsafe { sys::AMediaCodec_getOutputFormat(self.codec) };
        (!format.is_null()).then_some(Format(format))
    }

    fn input_format(&self) -> Option<Format> {
        type GetInputFormat = unsafe extern "C" fn(*mut sys::AMediaCodec) -> *mut sys::AMediaFormat;
        let call: GetInputFormat = later(c"AMediaCodec_getInputFormat")?;
        // SAFETY: the API 28 signature; the format returned is the caller's.
        let format = unsafe { call(self.codec) };
        (!format.is_null()).then_some(Format(format))
    }

    /// Asks for the next picture to be a sync frame, where the device takes it
    /// (API 26).
    fn request_sync(&self) {
        type SetParameters = unsafe extern "C" fn(*mut sys::AMediaCodec, *const sys::AMediaFormat) -> i32;
        let Some(call) = later::<SetParameters>(c"AMediaCodec_setParameters") else {
            return;
        };
        if let Ok(parameters) = Format::new() {
            parameters.int(c"request-sync", 0);
            // SAFETY: the API 26 signature, a live codec and format.
            unsafe { call(self.codec, parameters.0) };
        }
    }

    fn flush(&self) -> Result<()> {
        // SAFETY: a started codec.
        check(unsafe { sys::AMediaCodec_flush(self.codec) }, "flushing")
    }
}

impl Drop for Codec {
    fn drop(&mut self) {
        // SAFETY: owned outright; stopping a codec that never started is a
        // harmless error.
        unsafe {
            sys::AMediaCodec_stop(self.codec);
            sys::AMediaCodec_delete(self.codec);
        }
    }
}

/// The codec's name, where the device can say (API 28).
fn name_of(codec: *mut sys::AMediaCodec) -> Option<String> {
    type GetName = unsafe extern "C" fn(*mut sys::AMediaCodec, *mut *mut c_char) -> i32;
    type ReleaseName = unsafe extern "C" fn(*mut sys::AMediaCodec, *mut c_char);

    let get: GetName = later(c"AMediaCodec_getName")?;
    let release: Option<ReleaseName> = later(c"AMediaCodec_releaseName");

    let mut name = std::ptr::null_mut();
    // SAFETY: the API 28 signatures; the name is released as the NDK asks.
    unsafe {
        if get(codec, &mut name) != 0 || name.is_null() {
            return None;
        }
        let owned = CStr::from_ptr(name).to_string_lossy().into_owned();
        if let Some(release) = release {
            release(codec, name);
        }
        Some(owned)
    }
}

/// Google's software codecs, by the names AOSP gives them.
fn is_software(name: &str) -> bool {
    name.starts_with("OMX.google.") || name.starts_with("c2.android.")
}

fn check(status: sys::media_status_t, doing: &str) -> Result<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(Error::Encode {
            reason: format!("MediaCodec, {doing}: status {status}"),
        })
    }
}

/// H.264 through MediaCodec, to §15's spec.
pub struct MediaCodecEncoder {
    codec: Codec,
    width: u32,
    height: u32,
    /// The input buffer's layout: bytes a row, and rows before the chroma.
    stride: usize,
    slice_height: usize,
    keyframe_frames: u64,
    frames_in: u64,
    record: Option<Vec<u8>>,
    packets: Vec<Packet>,
    finished: bool,
}

impl MediaCodecEncoder {
    /// An H.264 encoder for `config`.
    ///
    /// # Errors
    ///
    /// [`Error::NoEncoder`] for anything but H.264, or a device without the
    /// codec asked for; [`Error::Encode`] if it will not take the settings.
    pub fn new(config: &EncoderConfig) -> Result<Self> {
        if config.encoding != Encoding::H264 {
            return Err(Error::NoEncoder {
                encoding: config.encoding,
                remedy: "MediaCodec writes H.264 for vtome; AV1 is rav1e's".to_string(),
            });
        }

        let codec = Codec::open(true, config.hardware).map_err(|error| Error::NoEncoder {
            encoding: Encoding::H264,
            remedy: error.to_string(),
        })?;

        let format = Format::new()?;
        // SAFETY: a live format and NUL-terminated strings.
        unsafe { sys::AMediaFormat_setString(format.0, c"mime".as_ptr(), c"video/avc".as_ptr()) };
        format.int(c"width", config.width as i32);
        format.int(c"height", config.height as i32);
        format.int(c"color-format", sys::COLOR_NV12);
        format.int(c"bitrate", i32::try_from(config.bitrate()).unwrap_or(i32::MAX));
        format.int(c"bitrate-mode", sys::BITRATE_VBR);
        format.int(c"frame-rate", config.frame_rate.as_f64().round().max(1.0) as i32);
        format.int(
            c"i-frame-interval",
            config.keyframe_interval.as_secs_f64().round().max(1.0) as i32,
        );
        format.int(c"profile", sys::PROFILE_HIGH);
        if config.level == crate::encode::Level::L4_1 {
            format.int(c"level", sys::LEVEL_4_1);
        }
        format.int(c"max-bframes", 0);

        codec.configure(&format, sys::CONFIGURE_FLAG_ENCODE)?;

        let (stride, slice_height) = codec
            .input_format()
            .map(|input| {
                (
                    input.get(c"stride").unwrap_or(config.width as i32),
                    input.get(c"slice-height").unwrap_or(config.height as i32),
                )
            })
            .unwrap_or((config.width as i32, config.height as i32));

        Ok(MediaCodecEncoder {
            codec,
            width: config.width,
            height: config.height,
            stride: (stride.max(config.width as i32)) as usize,
            slice_height: (slice_height.max(config.height as i32)) as usize,
            keyframe_frames: u64::from(config.keyframe_frames()),
            frames_in: 0,
            record: None,
            packets: Vec::new(),
            finished: false,
        })
    }

    /// Takes whatever output is ready, waiting up to `timeout` for the first.
    fn take_output(&mut self, timeout: Duration) {
        let mut wait = timeout;

        while let Some((bytes, info)) = self.codec.dequeue(wait) {
            wait = Duration::ZERO;

            if info.flags & sys::BUFFER_FLAG_END_OF_STREAM != 0 {
                self.finished = true;
            }

            if bytes.is_empty() {
                if self.finished {
                    break;
                }
                continue;
            }

            let unit = AccessUnit::split(&bytes);

            if self.record.is_none() {
                if let (Some(sps), Some(pps)) = (unit.sps.clone(), unit.pps.clone()) {
                    self.record = Some(AvcConfig::new(sps, pps).to_record());
                }
            }

            // The codec-config buffer is the parameter sets and nothing else.
            if info.flags & sys::BUFFER_FLAG_CODEC_CONFIG != 0 || unit.data.is_empty() {
                continue;
            }

            let pts = Duration::from_micros(info.presentation_time_us.max(0) as u64);
            self.packets.push(Packet {
                track_id: 1,
                data: unit.data,
                pts,
                dts: pts,
                is_keyframe: unit.is_keyframe || info.flags & sys::BUFFER_FLAG_KEY_FRAME != 0,
            });

            if self.finished {
                break;
            }
        }
    }

    /// NV12 laid out the way the codec's input buffer wants it.
    fn layout(&self, frame: &Frame) -> Result<Vec<u8>> {
        let nv12 = crate::scale::to_nv12(frame)?;
        let mut data = vec![0_u8; self.stride * self.slice_height * 3 / 2];

        for row in 0..self.height {
            let source = nv12.row(0, row).unwrap_or_default();
            let start = row as usize * self.stride;
            data[start..start + source.len()].copy_from_slice(source);
        }

        let chroma = self.stride * self.slice_height;
        for row in 0..self.height.div_ceil(2) {
            let source = nv12.row(1, row).unwrap_or_default();
            let start = chroma + row as usize * self.stride;
            data[start..start + source.len()].copy_from_slice(source);
        }

        Ok(data)
    }
}

impl Encoder for MediaCodecEncoder {
    fn encoding(&self) -> Encoding {
        Encoding::H264
    }

    fn backend(&self) -> Backend {
        Backend::MediaCodec
    }

    fn is_hardware(&self) -> bool {
        self.codec.hardware
    }

    fn config_record(&self) -> Option<Vec<u8>> {
        self.record.clone()
    }

    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>> {
        if (frame.width(), frame.height()) != (self.width, self.height) {
            return Err(Error::Encode {
                reason: format!(
                    "a {}×{} picture handed to an encoder opened for {}×{}",
                    frame.width(),
                    frame.height(),
                    self.width,
                    self.height
                ),
            });
        }

        if self.frames_in.is_multiple_of(self.keyframe_frames) {
            self.codec.request_sync();
        }
        self.frames_in += 1;

        let data = self.layout(frame)?;

        while !self.codec.try_queue(&data, frame.pts(), 0)? {
            self.take_output(Duration::from_millis(10));
        }

        self.take_output(Duration::ZERO);
        Ok(std::mem::take(&mut self.packets))
    }

    fn finish(&mut self) -> Result<Vec<Packet>> {
        while !self.codec.try_queue(&[], Duration::ZERO, sys::BUFFER_FLAG_END_OF_STREAM)? {
            self.take_output(Duration::from_millis(10));
        }

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !self.finished && std::time::Instant::now() < deadline {
            self.take_output(Duration::from_millis(50));
        }

        Ok(std::mem::take(&mut self.packets))
    }
}

/// H.264 through MediaCodec's decoder, into NV12 or I420.
pub struct MediaCodecDecoder {
    codec: Codec,
    nal_length_size: usize,
    width: u32,
    height: u32,
    color: ColorSpace,
    layout: Layout,
    ready: VecDeque<Frame>,
    finished: bool,
}

/// How the decoder lays its pictures out, from its output format.
#[derive(Clone, Copy)]
struct Layout {
    format: PixelFormat,
    stride: usize,
    slice_height: usize,
}

impl MediaCodecDecoder {
    /// A decoder for the track `config` describes.
    ///
    /// # Errors
    ///
    /// [`Error::NoDecoder`] for anything but H.264, or a device without the
    /// codec asked for; [`Error::Decode`] for a track without parameter sets.
    pub fn new(config: &DecoderConfig, hardware: Hardware) -> Result<Self> {
        if config.encoding != Encoding::H264 {
            return Err(Error::NoDecoder {
                encoding: config.encoding,
                remedy: "vtome drives MediaCodec for H.264".to_string(),
            });
        }

        let avc = AvcConfig::parse(&config.extra_data)?;
        let codec = Codec::open(false, hardware).map_err(|error| Error::NoDecoder {
            encoding: Encoding::H264,
            remedy: error.to_string(),
        })?;

        let annex_b = |set: &Vec<u8>| [&[0, 0, 0, 1][..], set].concat();
        let sps = avc.sequence_parameter_sets.first().map(annex_b).unwrap_or_default();
        let pps = avc.picture_parameter_sets.first().map(annex_b).unwrap_or_default();

        let format = Format::new()?;
        // SAFETY: a live format, NUL-terminated keys, and buffers that live
        // for the calls (MediaCodec copies them).
        unsafe {
            sys::AMediaFormat_setString(format.0, c"mime".as_ptr(), c"video/avc".as_ptr());
            sys::AMediaFormat_setBuffer(format.0, c"csd-0".as_ptr(), sps.as_ptr().cast(), sps.len());
            sys::AMediaFormat_setBuffer(format.0, c"csd-1".as_ptr(), pps.as_ptr().cast(), pps.len());
        }
        format.int(c"width", config.width as i32);
        format.int(c"height", config.height as i32);
        format.int(c"color-format", sys::COLOR_NV12);

        codec.configure(&format, 0).map_err(|error| Error::Decode {
            encoding: Encoding::H264,
            reason: error.to_string(),
        })?;

        Ok(MediaCodecDecoder {
            codec,
            nal_length_size: avc.nal_length_size,
            width: config.width,
            height: config.height,
            color: config.color,
            layout: Layout {
                format: PixelFormat::Nv12,
                stride: config.width as usize,
                slice_height: config.height as usize,
            },
            ready: VecDeque::new(),
            finished: false,
        })
    }

    /// Re-reads the layout after the codec announced a new output format.
    fn reread(&mut self) -> Result<()> {
        let Some(format) = self.codec.output_format() else {
            return Ok(());
        };

        let color = format.get(c"color-format").unwrap_or(sys::COLOR_NV12);
        let pixel = match color {
            sys::COLOR_NV12 => PixelFormat::Nv12,
            sys::COLOR_I420 => PixelFormat::I420,
            other => {
                return Err(Error::Decode {
                    encoding: Encoding::H264,
                    reason: format!(
                        "this device's decoder hands back colour format {other:#x}, a vendor \
                         layout vtome cannot read; it reads NV12 and I420"
                    ),
                })
            }
        };

        self.layout = Layout {
            format: pixel,
            stride: format.get(c"stride").unwrap_or(self.width as i32).max(self.width as i32) as usize,
            slice_height: format
                .get(c"slice-height")
                .unwrap_or(self.height as i32)
                .max(self.height as i32) as usize,
        };

        Ok(())
    }

    fn take_output(&mut self, timeout: Duration) -> Result<()> {
        let mut wait = timeout;

        while let Some((bytes, info)) = self.codec.dequeue(wait) {
            wait = Duration::ZERO;

            if info.flags & sys::BUFFER_FLAG_END_OF_STREAM != 0 {
                self.finished = true;
            }

            if bytes.is_empty() {
                if self.finished {
                    break;
                }
                self.reread()?;
                continue;
            }

            let Layout {
                format,
                stride,
                slice_height,
            } = self.layout;

            let planes = match format {
                PixelFormat::I420 => vec![
                    Plane { offset: 0, stride },
                    Plane {
                        offset: stride * slice_height,
                        stride: stride / 2,
                    },
                    Plane {
                        offset: stride * slice_height + (stride / 2) * (slice_height / 2),
                        stride: stride / 2,
                    },
                ],
                _ => vec![
                    Plane { offset: 0, stride },
                    Plane {
                        offset: stride * slice_height,
                        stride,
                    },
                ],
            };

            let frame = Frame::with_planes(
                self.width,
                self.height,
                format,
                self.color,
                Duration::from_micros(info.presentation_time_us.max(0) as u64),
                planes,
                bytes,
            )?;
            self.ready.push_back(frame);

            if self.finished {
                break;
            }
        }

        Ok(())
    }
}

impl Decoder for MediaCodecDecoder {
    fn encoding(&self) -> Encoding {
        Encoding::H264
    }

    fn decode(&mut self, packet: &Packet) -> Result<Option<Frame>> {
        if !packet.is_empty() {
            let annex_b = length_prefixed_to_annex_b(&packet.data, self.nal_length_size)?;
            while !self.codec.try_queue(&annex_b, packet.pts, 0)? {
                self.take_output(Duration::from_millis(10))?;
            }
        }

        self.take_output(Duration::ZERO)?;
        Ok(self.ready.pop_front())
    }

    fn flush(&mut self) -> Result<Vec<Frame>> {
        while !self.codec.try_queue(&[], Duration::ZERO, sys::BUFFER_FLAG_END_OF_STREAM)? {
            self.take_output(Duration::from_millis(10))?;
        }

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !self.finished && std::time::Instant::now() < deadline {
            self.take_output(Duration::from_millis(50))?;
        }

        Ok(self.ready.drain(..).collect())
    }

    fn reset(&mut self) -> Result<()> {
        self.ready.clear();
        self.finished = false;
        self.codec.flush()
    }

    fn is_hardware(&self) -> bool {
        self.codec.hardware
    }
}
