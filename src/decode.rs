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
//! | Backend | Platform | What it decodes |
//! |---|---|---|
//! | VideoToolbox | macOS, iOS | H.264, HEVC, and AV1 on hardware that has it |
//! | Media Foundation | Windows | H.264, HEVC, VP9, AV1 |
//! | MediaCodec | Android | whatever the device ships |
//! | VA-API | Linux | whatever the driver exposes |
//! | dav1d | anywhere | AV1, in software |
//! | libvpx | anywhere | VP9 and VP8, in software |
//!
//! The platform decoders come first: they are hardware-accelerated, and the
//! patent licence for H.264 and HEVC is the operating system's rather than
//! ours. The software backends are the floor — they are what makes "vtome can
//! always play what vtome wrote" true everywhere.
//!
//! # None of the backends is implemented yet
//!
//! This module is the shape they plug into: the trait, the selection, and the
//! errors. [`open`] refuses honestly, naming the backend that would have taken
//! the work, rather than returning a decoder that produces nothing. See
//! `planning/TODO.md` §2.

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
    /// dav1d, in software, anywhere.
    Dav1d,
    /// libvpx, in software, anywhere.
    LibVpx,
}

impl Backend {
    /// Whether this backend uses dedicated hardware.
    pub fn is_hardware(self) -> bool {
        !matches!(self, Backend::Dav1d | Backend::LibVpx)
    }

    /// The cargo feature that compiles it in.
    pub fn feature(self) -> &'static str {
        match self {
            Backend::VideoToolbox
            | Backend::MediaFoundation
            | Backend::MediaCodec
            | Backend::VaApi => "decode-platform",
            Backend::Dav1d => "decode-av1",
            Backend::LibVpx => "decode-vp9",
        }
    }

    /// What it can decode, where it exists.
    pub fn handles(self, encoding: Encoding) -> bool {
        match self {
            // The platform decoders all take the patented pair, which is the
            // reason to prefer them; what else they take varies by version and
            // is asked at runtime rather than assumed here.
            Backend::VideoToolbox | Backend::MediaFoundation | Backend::MediaCodec => matches!(
                encoding,
                Encoding::H264 | Encoding::H265 | Encoding::Av1 | Encoding::Vp9
            ),
            Backend::VaApi => matches!(
                encoding,
                Encoding::H264 | Encoding::H265 | Encoding::Av1 | Encoding::Vp9 | Encoding::Vp8
            ),
            Backend::Dav1d => encoding == Encoding::Av1,
            Backend::LibVpx => matches!(encoding, Encoding::Vp9 | Encoding::Vp8),
        }
    }

    /// Whether this build compiled it in *and* it exists on this platform.
    pub fn is_available(self) -> bool {
        let compiled = match self.feature() {
            "decode-platform" => cfg!(feature = "decode-platform"),
            "decode-av1" => cfg!(feature = "decode-av1"),
            "decode-vp9" => cfg!(feature = "decode-vp9"),
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
            Backend::Dav1d | Backend::LibVpx => true,
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
            Backend::LibVpx => "libvpx",
        };

        formatter.write_str(name)
    }
}

/// Every backend, in the order [`open`] tries them.
///
/// Hardware first: it is faster, cooler, and — for H.264 and HEVC — the only
/// path that does not raise a licensing question.
pub const BACKENDS: [Backend; 6] = [
    Backend::VideoToolbox,
    Backend::MediaFoundation,
    Backend::MediaCodec,
    Backend::VaApi,
    Backend::Dav1d,
    Backend::LibVpx,
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
    let candidates = backends_for(config.encoding);

    let Some(backend) = candidates.first().copied() else {
        return Err(Error::NoDecoder {
            encoding: config.encoding,
            remedy: remedy_for(config.encoding),
        });
    };

    // Every backend is scaffolding today. This is deliberately an error rather
    // than a decoder that returns no frames: a silent black window is the
    // hardest kind of bug to find, and there is no honest picture to return.
    Err(Error::NoDecoder {
        encoding: config.encoding,
        remedy: format!(
            "{backend} would take this and is not implemented yet \
             (planning/TODO.md §2); nothing decodes {} in this build",
            config.encoding
        ),
    })
}

/// What a person could do about there being no decoder.
fn remedy_for(encoding: Encoding) -> String {
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
        assert!(Backend::Dav1d.exists_here() && Backend::LibVpx.exists_here());
    }

    #[test]
    fn hardware_backends_are_marked_as_such() {
        assert!(Backend::VideoToolbox.is_hardware());
        assert!(!Backend::Dav1d.is_hardware());
    }

    #[test]
    fn only_the_platform_backends_offer_the_patented_encodings() {
        assert!(!Backend::Dav1d.handles(Encoding::H264));
        assert!(!Backend::LibVpx.handles(Encoding::H265));
        assert!(Backend::VideoToolbox.handles(Encoding::H264));
    }

    /// The error has to distinguish "you did not compile it" from "this machine
    /// does not have it", because they are different problems.
    #[test]
    fn a_missing_decoder_names_the_feature_that_would_have_supplied_it() {
        let Err(Error::NoDecoder { remedy, .. }) = open(&config(Encoding::Av1)) else {
            panic!("no AV1 decoder is implemented yet");
        };

        assert!(
            remedy.contains("dav1d") || remedy.contains("decode-av1") || remedy.contains("§2"),
            "{remedy}"
        );
    }

    #[test]
    fn an_encoding_nothing_here_can_decode_says_to_transcode_instead() {
        let remedy = remedy_for(Encoding::Theora);

        assert!(remedy.contains("transcode"), "{remedy}");
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
        if !cfg!(any(
            feature = "decode-av1",
            feature = "decode-vp9",
            feature = "decode-platform"
        )) {
            assert!(backends_for(Encoding::Av1).is_empty());
            assert!(backends_for(Encoding::H264).is_empty());
        }
    }
}
