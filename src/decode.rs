//! Turning compressed packets into pictures.
//!
//! One trait, several backends, and a runtime choice between them. The choice
//! is the interesting part: which decoder exists depends on the platform *and*
//! on what was compiled in, and the two fail differently. A build without
//! `decode-av1` and a Linux box without VA-API both produce "no decoder", and a
//! person needs to know which one they are looking at — so [`open`] says.
//!
//! # Where the decoders come from
//!
//! vtome reads two encodings, H.264 and AV1 ([`Encoding::is_in_scope`]);
//! anything else is refused as out of scope before a backend is asked.
//!
//! | Backend | Platform | What it decodes for vtome |
//! |---|---|---|
//! | VideoToolbox | macOS, iOS | H.264, and AV1 on hardware that has it |
//! | Media Foundation | Windows | H.264, AV1 |
//! | MediaCodec | Android | H.264, and AV1 where the device ships it |
//! | VA-API | Linux | whatever the driver exposes |
//! | dav1d | anywhere | AV1, in software |
//!
//! The platform decoders come first: they are hardware-accelerated, and the
//! patent licence for H.264 is the operating system's rather than ours. dav1d
//! is the floor — it is what makes "vtome can always play AV1, which it
//! writes" true everywhere.
//!
//! # What is implemented
//!
//! H.264 through VideoToolbox (Apple targets), Media Foundation (Windows), and
//! MediaCodec (Android), each with `decode-platform`. For everything else [`open`] refuses
//! honestly, naming the backend that would have taken the work, rather than
//! returning a decoder that produces nothing. See `planning/TODO.md` §2.

use crate::color::ColorSpace;
use crate::error::{Error, Result};
use crate::frame::Frame;
use crate::identify::Encoding;
use crate::media::{Packet, TrackInfo};

/// Something that turns packets into frames.
///
/// Decoders are stateful and are not `Sync`: one belongs to one thread, which
/// is how every platform decoder works anyway. They are `Send` so that thread
/// need not be the one that opened the file.
pub trait Decoder: Send {
    /// What this decodes.
    fn encoding(&self) -> Encoding;

    /// Feeds one packet in.
    ///
    /// Returns `None` when the decoder has taken the packet but has no picture
    /// yet, which is normal: B-frames mean output lags input by a frame or
    /// more, and a hardware decoder may hold several.
    fn decode(&mut self, packet: &Packet) -> Result<Option<Frame>>;

    /// Everything still held inside, at the end of a stream.
    ///
    /// Skipping this loses the last few frames of every file — the ones the
    /// decoder was holding for reordering.
    fn flush(&mut self) -> Result<Vec<Frame>>;

    /// Throws away all state, for a seek.
    fn reset(&mut self) -> Result<()>;

    /// Whether the pictures come from silicon dedicated to the job.
    ///
    /// Worth surfacing: it is the difference between a laptop playing 4K for an
    /// afternoon and one playing it until the battery runs out.
    fn is_hardware(&self) -> bool;
}

/// Whether a codec may, must, or must not use dedicated hardware — for
/// decoding and for encoding alike.
///
/// Hardware is faster, cooler, and kinder to a laptop battery; software is
/// predictable, identical from one GPU vendor to the next, and the way round a
/// hardware codec that is busy or misbehaving. Either way the codec is the
/// operating system's for H.264 — Apple's software H.264 encoder is still
/// Apple's, licence and all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Hardware {
    /// Hardware where the machine has it, software where it does not.
    #[default]
    Prefer,
    /// Hardware or nothing: a clear refusal rather than a quiet slow path.
    Require,
    /// Software throughout.
    Off,
}

/// Which implementation decodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Backend {
    /// Apple's, on macOS and iOS.
    VideoToolbox,
    /// Microsoft's, on Windows.
    MediaFoundation,
    /// Google's, on Android.
    MediaCodec,
    /// The Linux hardware path.
    VaApi,
    /// dav1d, in software, for AV1 anywhere.
    Dav1d,
}

impl Backend {
    /// Whether this backend uses dedicated hardware.
    pub fn is_hardware(self) -> bool {
        self != Backend::Dav1d
    }

    /// The cargo feature that compiles it in.
    pub fn feature(self) -> &'static str {
        match self {
            Backend::VideoToolbox
            | Backend::MediaFoundation
            | Backend::MediaCodec
            | Backend::VaApi => "decode-platform",
            Backend::Dav1d => "decode-av1",
        }
    }

    /// What it can decode for vtome, where it exists.
    pub fn handles(self, encoding: Encoding) -> bool {
        match self {
            // The platform decoders all take H.264, which is the reason to
            // prefer them; whether one takes AV1 depends on the hardware and is
            // asked at runtime rather than assumed here.
            Backend::VideoToolbox
            | Backend::MediaFoundation
            | Backend::MediaCodec
            | Backend::VaApi => matches!(encoding, Encoding::H264 | Encoding::Av1),
            Backend::Dav1d => encoding == Encoding::Av1,
        }
    }

    /// Whether this build compiled it in *and* it exists on this platform.
    pub fn is_available(self) -> bool {
        let compiled = match self.feature() {
            "decode-platform" => cfg!(feature = "decode-platform"),
            "decode-av1" => cfg!(feature = "decode-av1"),
            _ => false,
        };

        compiled && self.exists_here()
    }

    /// Whether the platform half is satisfied, ignoring features.
    fn exists_here(self) -> bool {
        match self {
            Backend::VideoToolbox => cfg!(target_vendor = "apple"),
            Backend::MediaFoundation => cfg!(windows),
            Backend::MediaCodec => cfg!(target_os = "android"),
            Backend::VaApi => cfg!(all(
                unix,
                not(target_vendor = "apple"),
                not(target_os = "android")
            )),
            Backend::Dav1d => true,
        }
    }
}

impl std::fmt::Display for Backend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Backend::VideoToolbox => "VideoToolbox",
            Backend::MediaFoundation => "Media Foundation",
            Backend::MediaCodec => "MediaCodec",
            Backend::VaApi => "VA-API",
            Backend::Dav1d => "dav1d",
        };

        formatter.write_str(name)
    }
}

/// Every backend, in the order [`open`] tries them.
///
/// Hardware first: it is faster, cooler, and — for H.264 — the only path that
/// does not raise a licensing question. Software fills the gaps.
pub const BACKENDS: [Backend; 5] = [
    Backend::VideoToolbox,
    Backend::MediaFoundation,
    Backend::MediaCodec,
    Backend::VaApi,
    Backend::Dav1d,
];

/// What a decoder needs to know before the first packet.
#[derive(Clone, Debug)]
pub struct DecoderConfig {
    /// What to decode.
    pub encoding: Encoding,
    /// Picture width.
    pub width: u32,
    /// Picture height.
    pub height: u32,
    /// Bits per sample.
    pub bit_depth: u32,
    /// What the samples will mean.
    pub color: ColorSpace,
    /// The codec's configuration record — `avcC`, `hvcC`, `av1C`. Without it a
    /// decoder cannot start; see [`crate::bitstream`].
    pub extra_data: Vec<u8>,
}

impl DecoderConfig {
    /// The configuration a demuxed track implies.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] if the track's encoding is not one this crate
    /// knows at all, which is different from having no decoder for it.
    pub fn from_track(track: &TrackInfo) -> Result<Self> {
        let encoding = track.encoding.ok_or_else(|| {
            Error::unsupported(format!(
                "track {} is {:?}, which vtome does not recognise as a video encoding",
                track.id, track.codec_id
            ))
        })?;

        Ok(DecoderConfig {
            encoding,
            width: track.width,
            height: track.height,
            bit_depth: track.bit_depth,
            color: track.color,
            extra_data: track.extra_data.clone(),
        })
    }
}

/// Which backends could decode this, here, in this build.
pub fn backends_for(encoding: Encoding) -> Vec<Backend> {
    BACKENDS
        .into_iter()
        .filter(|backend| backend.handles(encoding) && backend.is_available())
        .collect()
}

/// A decoder for this configuration.
///
/// # Errors
///
/// [`Error::NoDecoder`], whose `remedy` names the feature to turn on or the
/// platform that would have handled it — the two ways this fails are not the
/// same problem and should not read alike.
pub fn open(config: &DecoderConfig) -> Result<Box<dyn Decoder>> {
    open_with(config, Hardware::Prefer)
}

/// [`open`], with a say in whether the decoder runs in hardware.
///
/// A platform backend takes the preference itself — VideoToolbox decodes in
/// hardware or in software as asked. A software-only one, dav1d, is skipped
/// under [`Hardware::Require`].
///
/// # Errors
///
/// As [`open`]; and whatever a platform decoder says when it is told to use
/// hardware the machine does not have.
pub fn open_with(config: &DecoderConfig, hardware: Hardware) -> Result<Box<dyn Decoder>> {
    if !config.encoding.is_in_scope() {
        return Err(Error::NoDecoder {
            encoding: config.encoding,
            remedy: out_of_scope(config.encoding),
        });
    }

    let candidates: Vec<Backend> = backends_for(config.encoding)
        .into_iter()
        .filter(|backend| hardware != Hardware::Require || backend.is_hardware())
        .collect();

    let Some(first) = candidates.first().copied() else {
        if hardware == Hardware::Require && !backends_for(config.encoding).is_empty() {
            return Err(Error::NoDecoder {
                encoding: config.encoding,
                remedy: "hardware decoding was required, and only a software decoder for it \
                         is available here"
                    .to_string(),
            });
        }

        return Err(Error::NoDecoder {
            encoding: config.encoding,
            remedy: remedy_for(config.encoding),
        });
    };

    // In order, keeping the most recent refusal: a hardware decoder that turns
    // this stream down is a reason to try software, not to give up — and if
    // everything refuses, the refusal is more use than "not implemented".
    let mut refusal = None;

    for backend in candidates {
        match instantiate(backend, config, hardware) {
            Some(Ok(decoder)) => return Ok(decoder),
            Some(Err(error)) => refusal = Some(error),
            None => {}
        }
    }

    Err(refusal.unwrap_or_else(|| Error::NoDecoder {
        encoding: config.encoding,
        remedy: format!(
            "{first} would take this and is not implemented yet \
             (planning/TODO.md §2); nothing decodes {} in this build",
            config.encoding
        ),
    }))
}

/// The decoder one backend makes for `config`, or `None` where that backend is
/// not implemented yet.
// `config` goes unread in a build with no decoder feature.
#[allow(unused_variables)]
fn instantiate(
    backend: Backend,
    config: &DecoderConfig,
    hardware: Hardware,
) -> Option<Result<Box<dyn Decoder>>> {
    match backend {
        #[cfg(all(feature = "decode-platform", target_vendor = "apple"))]
        Backend::VideoToolbox => Some(
            crate::decode_videotoolbox::VideoToolboxDecoder::with_hardware(config, hardware)
                .map(|decoder| Box::new(decoder) as Box<dyn Decoder>),
        ),

        #[cfg(all(feature = "decode-platform", windows))]
        Backend::MediaFoundation => Some(
            crate::media_foundation::MediaFoundationDecoder::new(config, hardware)
                .map(|decoder| Box::new(decoder) as Box<dyn Decoder>),
        ),

        #[cfg(all(feature = "decode-platform", target_os = "android"))]
        Backend::MediaCodec => Some(
            crate::media_codec::MediaCodecDecoder::new(config, hardware)
                .map(|decoder| Box::new(decoder) as Box<dyn Decoder>),
        ),

        #[cfg(feature = "decode-av1")]
        Backend::Dav1d => Some(
            crate::decode_av1::Av1Decoder::new()
                .map(|decoder| Box::new(decoder) as Box<dyn Decoder>),
        ),

        _ => None,
    }
}

/// Why vtome will not read an encoding at all — a scope decision, not a
/// missing feature, and worded so it does not read like one.
pub(crate) fn out_of_scope(encoding: Encoding) -> String {
    format!(
        "{encoding} is outside vtome's scope: it reads H.264 and AV1 only. Convert it to \
         one of those with another tool first"
    )
}

/// What a person could do about there being no decoder.
fn remedy_for(encoding: Encoding) -> String {
    if !encoding.is_in_scope() {
        return out_of_scope(encoding);
    }

    // Which backends *would* have handled it, had they been compiled in?
    let missing: Vec<Backend> = BACKENDS
        .into_iter()
        .filter(|backend| backend.handles(encoding) && backend.exists_here())
        .collect();

    if missing.is_empty() {
        return format!(
            "nothing on this platform decodes {encoding}, and vtome has no software \
             fallback for it — transcode to AV1 first"
        );
    }

    let features: Vec<&str> = {
        let mut features: Vec<&str> = missing.iter().map(|backend| backend.feature()).collect();
        features.sort_unstable();
        features.dedup();
        features
    };

    format!(
        "enable the {} feature{} ({} would handle it here)",
        features.join(" or "),
        if features.len() == 1 { "" } else { "s" },
        missing
            .iter()
            .map(Backend::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// A stub decoder for Phase 1 testing — returns empty frames.
///
/// This allows the architecture to be tested without waiting for real AV1 decoder
/// implementation. Later, this will be replaced with actual decoders.
pub struct StubDecoder;

impl Decoder for StubDecoder {
    fn encoding(&self) -> Encoding {
        Encoding::Av1  // Stub: pretend it's AV1
    }

    fn decode(&mut self, _packet: &Packet) -> Result<Option<Frame>> {
        // Stub: no actual decoding, return None to indicate buffering
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

#[cfg(test)]
mod tests {
    use super::*;

    fn config(encoding: Encoding) -> DecoderConfig {
        DecoderConfig {
            encoding,
            width: 1920,
            height: 1080,
            bit_depth: 8,
            color: ColorSpace::default(),
            extra_data: Vec::new(),
        }
    }

    /// Exactly one platform backend exists on any given machine, and the
    /// software ones exist everywhere.
    #[test]
    fn the_platform_backends_are_mutually_exclusive() {
        let platform = [
            Backend::VideoToolbox,
            Backend::MediaFoundation,
            Backend::MediaCodec,
            Backend::VaApi,
        ]
        .into_iter()
        .filter(|backend| backend.exists_here())
        .count();

        assert!(platform <= 1, "{platform} platform decoders on one machine");
        assert!(Backend::Dav1d.exists_here());
    }

    #[test]
    fn hardware_backends_are_marked_as_such() {
        assert!(Backend::VideoToolbox.is_hardware());
        assert!(!Backend::Dav1d.is_hardware());
    }

    #[test]
    fn only_the_platform_backends_offer_the_patented_encodings() {
        assert!(!Backend::Dav1d.handles(Encoding::H264));
        assert!(Backend::VideoToolbox.handles(Encoding::H264));

        // Out of scope, so no backend is asked — not even one that could.
        for backend in BACKENDS {
            assert!(!backend.handles(Encoding::H265), "{backend}");
            assert!(!backend.handles(Encoding::Vp9), "{backend}");
        }
    }

    /// The error has to distinguish "you did not compile it" from "this machine
    /// does not have it", because they are different problems.
    #[test]
    fn an_out_of_scope_encoding_is_refused_as_a_scope_decision() {
        for encoding in [Encoding::Vp9, Encoding::H265, Encoding::Theora] {
            let Err(Error::NoDecoder { remedy, .. }) = open(&config(encoding)) else {
                panic!("{encoding} is outside vtome's scope");
            };

            // Not "enable a feature": no feature would help.
            assert!(remedy.contains("scope"), "{remedy}");
            assert!(!remedy.contains("feature"), "{remedy}");
        }
    }

    /// The error has to distinguish "you did not compile it" from "this machine
    /// does not have it", because they are different problems.
    #[test]
    fn a_missing_decoder_names_the_feature_that_would_have_supplied_it() {
        // dav1d exists everywhere, so AV1 without `decode-av1` is a feature to
        // turn on rather than a platform to change.
        if cfg!(feature = "decode-av1") {
            return;
        }

        let remedy = remedy_for(Encoding::Av1);
        assert!(remedy.contains("decode-av1"), "{remedy}");
    }

    #[test]
    fn a_track_with_no_recognised_encoding_is_a_different_error_from_no_decoder() {
        let track = TrackInfo {
            id: 3,
            kind: crate::media::TrackKind::Video,
            encoding: None,
            codec_id: "V_SOMETHING_ELSE".to_string(),
            width: 320,
            height: 240,
            frame_rate: None,
            bit_depth: 8,
            color: ColorSpace::default(),
            rotation: Default::default(),
            duration: std::time::Duration::ZERO,
            extra_data: Vec::new(),
        };

        assert!(matches!(
            DecoderConfig::from_track(&track),
            Err(Error::Unsupported { .. })
        ));
    }

    #[test]
    fn nothing_is_available_without_its_feature() {
        // The default build turns none of the decoder features on, so this is
        // the honest state of the crate today.
        if !cfg!(any(feature = "decode-av1", feature = "decode-platform")) {
            assert!(backends_for(Encoding::Av1).is_empty());
            assert!(backends_for(Encoding::H264).is_empty());
        }
    }
}
