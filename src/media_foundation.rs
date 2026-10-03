//! Media Foundation: the H.264 encoder and decoder Windows ships.
//!
//! The Windows counterpart of VideoToolbox, and for the same reason: the codec
//! is the operating system's, so the H.264 licence is Microsoft's. Both
//! directions go through a Media Foundation Transform (MFT), found with
//! `MFTEnumEx` — a vendor's hardware MFT where the machine has one and
//! [`Hardware`] allows it, Microsoft's software MFT otherwise.
//!
//! # Two ways an MFT runs
//!
//! Microsoft's software MFTs are synchronous: hand in a sample, ask for output
//! until it says it needs more. Hardware MFTs are asynchronous: they say when
//! they want input and when they have output through events, and must be
//! unlocked to be used at all. [`Transform`] speaks both, so the encoder and
//! decoder above it never know which they got.
//!
//! # The encoder is set to §15
//!
//! High profile, Level 4.1 on the output type; no B-frames, CABAC, a GOP of
//! two seconds, and quality-based rate control at 68 through `ICodecAPI`, with
//! every two-second frame forced to a keyframe so the cadence is the spec's.
//! Its Annex B output is taken apart into MP4's shape by
//! [`AccessUnit`](crate::bitstream::AccessUnit).
//!
//! Compiled and type-checked for Windows on every change; run on Windows by
//! hand — no container here can host Media Foundation.

// Media Foundation's event names, matched as Microsoft spells them.
#![allow(non_upper_case_globals)]

use std::cell::Cell;
use std::collections::VecDeque;
use std::time::Duration;

use windows::core::{Interface, GUID};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::VARIANT;

use crate::bitstream::{length_prefixed_to_annex_b, AccessUnit, AvcConfig};
use crate::color::{ColorSpace, Range};
use crate::decode::{Decoder, DecoderConfig, Hardware};
use crate::encode::{Backend, Encoder, EncoderConfig};
use crate::error::{Error, Result};
use crate::frame::{Frame, PixelFormat, Plane};
use crate::identify::Encoding;
use crate::media::Packet;

/// Media Foundation counts time in 100-nanosecond ticks.
fn ticks(time: Duration) -> i64 {
    i64::try_from(time.as_nanos() / 100).unwrap_or(i64::MAX)
}

fn from_ticks(ticks: i64) -> Duration {
    Duration::from_nanos(u64::try_from(ticks).unwrap_or(0).saturating_mul(100))
}

/// COM on this thread, and Media Foundation in this process — both counted, so
/// asking again is free.
fn ensure_started() -> Result<()> {
    thread_local! {
        static COM: Cell<bool> = const { Cell::new(false) };
    }

    COM.with(|done| {
        if !done.get() {
            // Already initialised in another mode is fine: MFTs are free-threaded.
            // SAFETY: no reserved pointer; the flag is a valid COINIT.
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            done.set(true);
        }
    });

    static STARTED: std::sync::OnceLock<std::result::Result<(), String>> = std::sync::OnceLock::new();

    STARTED
        .get_or_init(|| {
            // SAFETY: the version this SDK was built against; started once and
            // left running for the life of the process.
            unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) }.map_err(|error| error.to_string())
        })
        .clone()
        .map_err(|reason| Error::unsupported(format!("Media Foundation would not start: {reason}")))
}

fn failed(doing: &str, error: windows::core::Error) -> Error {
    Error::Encode {
        reason: format!("Media Foundation, {doing}: {error}"),
    }
}

fn decode_failed(doing: &str, error: windows::core::Error) -> Error {
    Error::Decode {
        encoding: Encoding::H264,
        reason: format!("Media Foundation, {doing}: {error}"),
    }
}

/// An MFT, synchronous or asynchronous, behind one interface.
struct Transform {
    mft: IMFTransform,
    /// Present for an asynchronous (hardware) MFT.
    events: Option<IMFMediaEventGenerator>,
    hardware: bool,
    /// Inputs an asynchronous MFT has asked for and not yet been given.
    wanted: u32,
}

// SAFETY: MFTs are free-threaded COM objects; `&mut self` on every call keeps
// them to one caller at a time.
unsafe impl Send for Transform {}

impl Transform {
    /// The first MFT of `category` that turns `input` into `output`, as
    /// `hardware` allows: hardware tried first under `Prefer`, only under
    /// `Require`, never under `Off`.
    fn find(category: GUID, input: GUID, output: GUID, hardware: Hardware) -> Result<Self> {
        ensure_started()?;

        let passes: &[bool] = match hardware {
            Hardware::Prefer => &[true, false],
            Hardware::Require => &[true],
            Hardware::Off => &[false],
        };

        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: input,
        };
        let output = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: output,
        };

        for &in_hardware in passes {
            let flags = if in_hardware {
                MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0)
            } else {
                MFT_ENUM_FLAG(MFT_ENUM_FLAG_SYNCMFT.0 | MFT_ENUM_FLAG_SORTANDFILTER.0)
            };

            let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut count = 0_u32;

            // SAFETY: both type infos live for the call; MF allocates `list`
            // with CoTaskMemAlloc, freed below.
            let found = unsafe {
                MFTEnumEx(category, flags, Some(&input), Some(&output), &mut list, &mut count)
            };

            if found.is_err() || list.is_null() {
                continue;
            }

            // SAFETY: MF wrote `count` activation objects at `list`.
            let activates: Vec<Option<IMFActivate>> =
                (0..count as usize).map(|index| unsafe { list.add(index).read() }).collect();
            // SAFETY: the array itself, now that its entries are owned here.
            unsafe { CoTaskMemFree(Some(list as *const _)) };

            for activate in activates.into_iter().flatten() {
                // SAFETY: a live activation object.
                let Ok(mft) = (unsafe { activate.ActivateObject::<IMFTransform>() }) else {
                    continue;
                };

                let events = if in_hardware {
                    // SAFETY: a live MFT; unlocking is what an asynchronous
                    // MFT requires before any other call.
                    unsafe {
                        if let Ok(attributes) = mft.GetAttributes() {
                            let _ = attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
                        }
                    }
                    mft.cast::<IMFMediaEventGenerator>().ok()
                } else {
                    None
                };

                return Ok(Transform {
                    mft,
                    events,
                    hardware: in_hardware,
                    wanted: 0,
                });
            }
        }

        Err(Error::unsupported(match hardware {
            Hardware::Require => "Media Foundation has no hardware MFT for this here",
            _ => "Media Foundation has no MFT for this here",
        }))
    }

    /// Tells the MFT streaming is starting.
    fn begin(&self) -> windows::core::Result<()> {
        // SAFETY: a live MFT with its types set.
        unsafe {
            self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
        }
    }

    /// One sample in; whatever output is ready, out.
    fn feed(&mut self, sample: &IMFSample) -> windows::core::Result<Vec<IMFSample>> {
        let mut outputs = Vec::new();

        if self.events.is_some() {
            // Wait to be asked, taking output as it is offered meanwhile.
            while self.wanted == 0 {
                self.next_event(true, &mut outputs)?;
            }
            self.wanted -= 1;

            // SAFETY: a live MFT that asked for input.
            unsafe { self.mft.ProcessInput(0, sample, 0)? };

            while self.next_event(false, &mut outputs)? {}
            return Ok(outputs);
        }

        loop {
            // SAFETY: a live, synchronous MFT.
            match unsafe { self.mft.ProcessInput(0, sample, 0) } {
                Ok(()) => break,
                // Full: take what it has, then hand the sample over again.
                Err(error) if error.code() == MF_E_NOTACCEPTING => {
                    if !self.pull(&mut outputs)? {
                        return Err(error);
                    }
                }
                Err(error) => return Err(error),
            }
        }

        while self.pull(&mut outputs)? {}
        Ok(outputs)
    }

    /// Everything still inside, at the end of the stream.
    fn drain(&mut self) -> windows::core::Result<Vec<IMFSample>> {
        let mut outputs = Vec::new();

        // SAFETY: a live MFT.
        unsafe {
            self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)?;
            self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
        }

        if self.events.is_some() {
            // Until the MFT says it has nothing more.
            while let Some(events) = &self.events {
                // SAFETY: a live event generator; blocking is what is wanted.
                let event = unsafe { events.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0))? };
                // SAFETY: a live event.
                match MF_EVENT_TYPE(unsafe { event.GetType()? } as i32) {
                    METransformHaveOutput => {
                        self.pull(&mut outputs)?;
                    }
                    METransformDrainComplete => break,
                    _ => {}
                }
            }
        } else {
            while self.pull(&mut outputs)? {}
        }

        Ok(outputs)
    }

    /// Throws away everything inside, for a seek.
    fn flush(&mut self) -> windows::core::Result<()> {
        self.wanted = 0;
        // SAFETY: a live MFT.
        unsafe {
            self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
            self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
        }
    }

    /// Handles one event of an asynchronous MFT. `false` when there was none
    /// waiting and `block` was not asked for.
    fn next_event(&mut self, block: bool, outputs: &mut Vec<IMFSample>) -> windows::core::Result<bool> {
        let Some(events) = &self.events else {
            return Ok(false);
        };

        let flags = if block {
            MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0)
        } else {
            MF_EVENT_FLAG_NO_WAIT
        };

        // SAFETY: a live event generator.
        let event = match unsafe { events.GetEvent(flags) } {
            Ok(event) => event,
            Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(false),
            Err(error) => return Err(error),
        };

        // SAFETY: a live event.
        match MF_EVENT_TYPE(unsafe { event.GetType()? } as i32) {
            METransformNeedInput => self.wanted += 1,
            METransformHaveOutput => {
                self.pull(outputs)?;
            }
            _ => {}
        }

        Ok(true)
    }

    /// One `ProcessOutput`. `false` once the MFT needs more input. A changed
    /// output format is renegotiated — to NV12 for a decoder — and retried.
    fn pull(&mut self, outputs: &mut Vec<IMFSample>) -> windows::core::Result<bool> {
        // SAFETY: a live MFT.
        let info = unsafe { self.mft.GetOutputStreamInfo(0)? };
        let provides = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;

        let sample = if provides {
            None
        } else {
            // SAFETY: plain allocations, owned here until handed over.
            unsafe {
                let sample = MFCreateSample()?;
                let buffer = MFCreateMemoryBuffer(info.cbSize.max(1 << 20))?;
                sample.AddBuffer(&buffer)?;
                Some(sample)
            }
        };

        let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: std::mem::ManuallyDrop::new(sample),
            dwStatus: 0,
            pEvents: std::mem::ManuallyDrop::new(None),
        }];
        let mut status = 0_u32;

        // SAFETY: one output buffer for the MFT's one output stream.
        let result = unsafe { self.mft.ProcessOutput(0, &mut buffers, &mut status) };

        // Ownership of what came back is ours either way.
        let [buffer] = buffers;
        let sample = std::mem::ManuallyDrop::into_inner(buffer.pSample);
        drop(std::mem::ManuallyDrop::into_inner(buffer.pEvents));

        match result {
            Ok(()) => {
                outputs.extend(sample);
                Ok(true)
            }
            Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(false),
            Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                self.renegotiate()?;
                Ok(true)
            }
            Err(error) => Err(error),
        }
    }

    /// After a stream change: the first NV12 output type on offer.
    fn renegotiate(&self) -> windows::core::Result<()> {
        let mut index = 0;

        loop {
            // SAFETY: a live MFT; running off the end of the list is an error,
            // which ends the search.
            let offered = unsafe { self.mft.GetOutputAvailableType(0, index)? };
            // SAFETY: a live media type.
            if unsafe { offered.GetGUID(&MF_MT_SUBTYPE)? } == MFVideoFormat_NV12 {
                // SAFETY: as above.
                return unsafe { self.mft.SetOutputType(0, &offered, 0) };
            }
            index += 1;
        }
    }

    fn codec_api(&self) -> Option<ICodecAPI> {
        self.mft.cast::<ICodecAPI>().ok()
    }
}

/// A sample holding `bytes`, at `time`.
fn sample_of(bytes: &[u8], time: Duration, duration: Duration) -> windows::core::Result<IMFSample> {
    // SAFETY: a fresh buffer, locked only to copy in exactly its own length.
    unsafe {
        let buffer = MFCreateMemoryBuffer(u32::try_from(bytes.len()).unwrap_or(u32::MAX))?;
        let mut data = std::ptr::null_mut();
        buffer.Lock(&mut data, None, None)?;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(bytes.len() as u32)?;

        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(ticks(time))?;
        sample.SetSampleDuration(ticks(duration))?;
        Ok(sample)
    }
}

/// A sample's bytes, and its time.
fn bytes_of(sample: &IMFSample) -> windows::core::Result<(Vec<u8>, Duration)> {
    // SAFETY: a live sample, locked only to copy out its current length.
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer()?;
        let mut data = std::ptr::null_mut();
        let mut length = 0_u32;
        buffer.Lock(&mut data, None, Some(&mut length))?;
        let bytes = std::slice::from_raw_parts(data, length as usize).to_vec();
        buffer.Unlock()?;

        Ok((bytes, from_ticks(sample.GetSampleTime().unwrap_or(0))))
    }
}

fn pack(high: u32, low: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
}

/// H.264 through Media Foundation, to §15's spec.
pub struct MediaFoundationEncoder {
    transform: Transform,
    width: u32,
    height: u32,
    frame: Duration,
    keyframe_frames: u64,
    frames_in: u64,
    record: Option<Vec<u8>>,
}

impl MediaFoundationEncoder {
    /// An H.264 encoder for `config`.
    ///
    /// # Errors
    ///
    /// [`Error::NoEncoder`] for anything but H.264 or for hardware Windows does
    /// not have; [`Error::Encode`] if the MFT will not take the settings.
    pub fn new(config: &EncoderConfig) -> Result<Self> {
        if config.encoding != Encoding::H264 {
            return Err(Error::NoEncoder {
                encoding: config.encoding,
                remedy: "Media Foundation writes H.264 for vtome; AV1 is rav1e's".to_string(),
            });
        }

        let transform = Transform::find(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFVideoFormat_NV12,
            MFVideoFormat_H264,
            config.hardware,
        )
        .map_err(|error| Error::NoEncoder {
            encoding: Encoding::H264,
            remedy: error.to_string(),
        })?;

        let rate = config.frame_rate;
        let size = pack(config.width, config.height);
        let frame_rate = pack(rate.numerator, rate.denominator.max(1));

        // SAFETY: fresh media types configured before they are handed to a
        // live MFT; the ICodecAPI values are plain integers and booleans.
        unsafe {
            let output = MFCreateMediaType().map_err(|e| failed("making a type", e))?;
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(|e| failed("the type", e))?;
            output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264).map_err(|e| failed("the type", e))?;
            output.SetUINT64(&MF_MT_FRAME_SIZE, size).map_err(|e| failed("the size", e))?;
            output.SetUINT64(&MF_MT_FRAME_RATE, frame_rate).map_err(|e| failed("the rate", e))?;
            output
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(|e| failed("progressive", e))?;
            output
                .SetUINT32(&MF_MT_AVG_BITRATE, config.bitrate())
                .map_err(|e| failed("the bitrate", e))?;
            output
                .SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)
                .map_err(|e| failed("High profile", e))?;
            if config.level == crate::encode::Level::L4_1 {
                output
                    .SetUINT32(&MF_MT_MPEG2_LEVEL, eAVEncH264VLevel4_1.0 as u32)
                    .map_err(|e| failed("Level 4.1", e))?;
            }
            transform.mft.SetOutputType(0, &output, 0).map_err(|e| failed("the output type", e))?;

            let input = MFCreateMediaType().map_err(|e| failed("making a type", e))?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(|e| failed("the type", e))?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).map_err(|e| failed("the type", e))?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, size).map_err(|e| failed("the size", e))?;
            input.SetUINT64(&MF_MT_FRAME_RATE, frame_rate).map_err(|e| failed("the rate", e))?;
            input
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(|e| failed("progressive", e))?;
            transform.mft.SetInputType(0, &input, 0).map_err(|e| failed("the input type", e))?;

            if let Some(api) = transform.codec_api() {
                // Each is the spec's; an encoder that declines one still
                // honours the type above, so a refusal is not fatal.
                let _ = api.SetValue(&CODECAPI_AVEncMPVGOPSize, &VARIANT::from(config.keyframe_frames()));
                let _ = api.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &VARIANT::from(0_u32));
                let _ = api.SetValue(&CODECAPI_AVEncH264CABACEnable, &VARIANT::from(true));
                let _ = api.SetValue(
                    &CODECAPI_AVEncCommonRateControlMode,
                    &VARIANT::from(eAVEncCommonRateControlMode_Quality.0 as u32),
                );
                let _ = api.SetValue(
                    &CODECAPI_AVEncCommonQuality,
                    &VARIANT::from((config.quality.clamp(0.0, 1.0) * 100.0).round() as u32),
                );
                if config.realtime {
                    let _ = api.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
                }
            }
        }

        transform.begin().map_err(|e| failed("starting", e))?;

        Ok(MediaFoundationEncoder {
            transform,
            width: config.width,
            height: config.height,
            frame: rate.frame_duration(),
            keyframe_frames: u64::from(config.keyframe_frames()),
            frames_in: 0,
            record: None,
        })
    }

    fn packets(&mut self, samples: Vec<IMFSample>) -> Result<Vec<Packet>> {
        samples
            .iter()
            .map(|sample| {
                let (bytes, pts) = bytes_of(sample).map_err(|e| failed("reading a packet", e))?;
                let unit = AccessUnit::split(&bytes);

                if self.record.is_none() {
                    if let (Some(sps), Some(pps)) = (unit.sps.clone(), unit.pps.clone()) {
                        self.record = Some(AvcConfig::new(sps, pps).to_record());
                    }
                }

                Ok(Packet {
                    track_id: 1,
                    data: unit.data,
                    pts,
                    dts: pts,
                    is_keyframe: unit.is_keyframe,
                })
            })
            .collect()
    }
}

impl Encoder for MediaFoundationEncoder {
    fn encoding(&self) -> Encoding {
        Encoding::H264
    }

    fn backend(&self) -> Backend {
        Backend::MediaFoundation
    }

    fn is_hardware(&self) -> bool {
        self.transform.hardware
    }

    fn config_record(&self) -> Option<Vec<u8>> {
        self.record.clone()
    }

    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>> {
        ensure_started()?;

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

        // The spec's cadence: every keyframe_frames'th picture forced.
        if self.frames_in.is_multiple_of(self.keyframe_frames) {
            if let Some(api) = self.transform.codec_api() {
                // SAFETY: a live codec API and a plain integer.
                let _ = unsafe { api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1_u32)) };
            }
        }
        self.frames_in += 1;

        let nv12 = crate::scale::to_nv12(frame)?;
        let sample = sample_of(nv12.data(), frame.pts(), self.frame).map_err(|e| failed("a picture", e))?;
        let outputs = self.transform.feed(&sample).map_err(|e| failed("encoding", e))?;

        self.packets(outputs)
    }

    fn finish(&mut self) -> Result<Vec<Packet>> {
        let outputs = self.transform.drain().map_err(|e| failed("finishing", e))?;
        self.packets(outputs)
    }
}

/// H.264 through Media Foundation's decoder, into NV12.
pub struct MediaFoundationDecoder {
    transform: Transform,
    /// The SPS and PPS in Annex B, sent ahead of the first packet and after
    /// every reset: MP4 keeps them out of band.
    parameter_sets: Vec<u8>,
    nal_length_size: usize,
    send_parameter_sets: bool,
    width: u32,
    height: u32,
    color: ColorSpace,
    /// Pictures out and not yet handed on.
    ready: VecDeque<Frame>,
}

impl MediaFoundationDecoder {
    /// A decoder for the track `config` describes.
    ///
    /// # Errors
    ///
    /// [`Error::NoDecoder`] for anything but H.264, or hardware Windows does
    /// not have; [`Error::Decode`] for a track without parameter sets or a
    /// type the MFT refuses.
    pub fn new(config: &DecoderConfig, hardware: Hardware) -> Result<Self> {
        if config.encoding != Encoding::H264 {
            return Err(Error::NoDecoder {
                encoding: config.encoding,
                remedy: "vtome drives Media Foundation for H.264".to_string(),
            });
        }

        let avc = AvcConfig::parse(&config.extra_data)?;

        let transform = Transform::find(
            MFT_CATEGORY_VIDEO_DECODER,
            MFVideoFormat_H264,
            MFVideoFormat_NV12,
            hardware,
        )
        .map_err(|error| Error::NoDecoder {
            encoding: Encoding::H264,
            remedy: error.to_string(),
        })?;

        // SAFETY: a fresh media type handed to a live MFT.
        unsafe {
            let input = MFCreateMediaType().map_err(|e| decode_failed("making a type", e))?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(|e| decode_failed("the type", e))?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264).map_err(|e| decode_failed("the type", e))?;
            input
                .SetUINT64(&MF_MT_FRAME_SIZE, pack(config.width, config.height))
                .map_err(|e| decode_failed("the size", e))?;
            transform.mft.SetInputType(0, &input, 0).map_err(|e| decode_failed("the input type", e))?;
        }

        transform.renegotiate().map_err(|e| decode_failed("an NV12 output", e))?;
        transform.begin().map_err(|e| decode_failed("starting", e))?;

        Ok(MediaFoundationDecoder {
            transform,
            parameter_sets: avc.to_annex_b(),
            nal_length_size: avc.nal_length_size,
            send_parameter_sets: true,
            width: config.width,
            height: config.height,
            color: config.color,
            ready: VecDeque::new(),
        })
    }

    /// An NV12 sample as a frame, cropped to the picture: the decoder's buffer
    /// is often padded to whole macroblocks, 1088 rows for 1080.
    fn frame(&self, sample: &IMFSample) -> Result<Frame> {
        let (bytes, pts) = bytes_of(sample).map_err(|e| decode_failed("reading a picture", e))?;

        // SAFETY: a live MFT with its output type set.
        let (stride, rows) = unsafe {
            let current = self.transform.mft.GetOutputCurrentType(0).map_err(|e| decode_failed("the output type", e))?;
            let size = current.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(pack(self.width, self.height));
            let stride = current.GetUINT32(&MF_MT_DEFAULT_STRIDE).unwrap_or((size >> 32) as u32);
            (stride as usize, (size & 0xFFFF_FFFF) as usize)
        };

        let range = if self.color.range == Range::Full { Range::Full } else { Range::Limited };

        Frame::with_planes(
            self.width,
            self.height,
            PixelFormat::Nv12,
            ColorSpace { range, ..self.color },
            pts,
            vec![
                Plane { offset: 0, stride },
                Plane {
                    offset: stride * rows.max(self.height as usize),
                    stride,
                },
            ],
            bytes,
        )
    }
}

impl Decoder for MediaFoundationDecoder {
    fn encoding(&self) -> Encoding {
        Encoding::H264
    }

    fn decode(&mut self, packet: &Packet) -> Result<Option<Frame>> {
        ensure_started()?;

        if packet.is_empty() {
            return Ok(None);
        }

        let mut annex_b = Vec::with_capacity(packet.len() + self.parameter_sets.len() + 16);
        if std::mem::take(&mut self.send_parameter_sets) {
            annex_b.extend_from_slice(&self.parameter_sets);
        }
        annex_b.extend(length_prefixed_to_annex_b(&packet.data, self.nal_length_size)?);

        let sample = sample_of(&annex_b, packet.pts, Duration::ZERO)
            .map_err(|e| decode_failed("a packet", e))?;
        let outputs = self.transform.feed(&sample).map_err(|e| decode_failed("decoding", e))?;

        // Media Foundation emits in display order; a packet can complete more
        // than one picture, and the rest wait their turn.
        for sample in &outputs {
            let frame = self.frame(sample)?;
            self.ready.push_back(frame);
        }

        Ok(self.ready.pop_front())
    }

    fn flush(&mut self) -> Result<Vec<Frame>> {
        let outputs = self.transform.drain().map_err(|e| decode_failed("draining", e))?;
        let mut frames: Vec<Frame> = self.ready.drain(..).collect();
        for sample in &outputs {
            frames.push(self.frame(sample)?);
        }
        Ok(frames)
    }

    fn reset(&mut self) -> Result<()> {
        self.ready.clear();
        self.transform.flush().map_err(|e| decode_failed("flushing", e))?;
        self.send_parameter_sets = true;
        Ok(())
    }

    fn is_hardware(&self) -> bool {
        self.transform.hardware
    }
}
