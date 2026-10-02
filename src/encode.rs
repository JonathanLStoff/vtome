//! Turning pictures into packets: H.264 through the operating system, AV1
//! through the bundled `rav1e`.
//!
//! The mirror of [`crate::decode`] — one trait, several backends, and a runtime
//! choice that says plainly why it chose nothing when it does.
//!
//! # Two formats, two rules
//!
//! | Encoding | Backend | Why this one |
//! |---|---|---|
//! | H.264 — the default | VideoToolbox (macOS, iOS) | Universal hardware decode, everywhere a file goes. Patent-pooled, so **only ever the OS's encoder** — never x264, never openh264. Media Foundation and MediaCodec take the same place on Windows and Android, later |
//! | AV1 | `rav1e`, bundled | Royalty-free and pure Rust, so it ships inside every transcode build. Where the OS has no H.264 encoder (Linux today), and whenever asked for |
//!
//! # What the files look like
//!
//! Every H.264 file vtome writes meets the universal baseline in
//! `planning/TODO.md` §15, because what a file declares decides whether a
//! hardware decoder takes it: High profile at **Level 4.1** (8,192 macroblocks
//! a frame and 245,760 a second — 1080p30 or 720p60 — so a bigger or faster
//! picture is scaled down to fit, see [`fit_level`]), 8-bit 4:2:0, no
//! B-frames, a keyframe every two seconds exactly so every GOP is closed, and
//! CABAC. AV1 follows the same shape where it applies: 8-bit 4:2:0, no frame
//! reordering, a keyframe every two seconds.

use std::time::Duration;

use crate::color::ColorSpace;
use crate::decode::Hardware;
use crate::error::{Error, Result};
use crate::frame::Frame;
use crate::identify::Encoding;
use crate::media::{Packet, Rational};

/// Something that turns frames into packets.
///
/// Like a [`Decoder`](crate::Decoder), one belongs to one thread at a time and
/// may be moved between them.
pub trait Encoder: Send {
    /// What this writes.
    fn encoding(&self) -> Encoding;

    /// Which implementation it is.
    fn backend(&self) -> Backend;

    /// Whether the encoding happens in dedicated hardware — asked of the
    /// encoder, not assumed from the backend.
    fn is_hardware(&self) -> bool;

    /// The record a container needs before the first sample: `avcC` for
    /// H.264, `av1C` for AV1. `None` until the encoder knows it — VideoToolbox
    /// says only once its first picture is out.
    fn config_record(&self) -> Option<Vec<u8>>;

    /// Feeds one picture in, and takes whatever packets are finished.
    ///
    /// The picture must be the size the encoder was opened for, in 8-bit
    /// 4:2:0 (I420 or NV12). Packets come out in presentation order — no
    /// encoder here reorders — stamped with the pictures' own timestamps.
    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>>;

    /// Everything still inside, at the end of the stream.
    fn finish(&mut self) -> Result<Vec<Packet>>;
}

/// Which implementation encodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Backend {
    /// Apple's, on macOS and iOS: H.264 in hardware, or in Apple's software
    /// encoder on a Mac when asked.
    VideoToolbox,
    /// Microsoft's, on Windows. Not implemented yet.
    MediaFoundation,
    /// Google's, on Android. Not implemented yet.
    MediaCodec,
    /// rav1e, in software, for AV1 anywhere.
    Rav1e,
}

impl Backend {
    /// Whether this backend can encode in dedicated hardware at all.
    pub fn can_use_hardware(self) -> bool {
        self != Backend::Rav1e
    }

    /// The cargo feature that compiles it in.
    pub fn feature(self) -> &'static str {
        match self {
            Backend::VideoToolbox | Backend::MediaFoundation | Backend::MediaCodec => {
                "encode-platform"
            }
            Backend::Rav1e => "encode-av1",
        }
    }

    /// What it writes for vtome.
    pub fn handles(self, encoding: Encoding) -> bool {
        match self {
            Backend::VideoToolbox | Backend::MediaFoundation | Backend::MediaCodec => {
                encoding == Encoding::H264
            }
            Backend::Rav1e => encoding == Encoding::Av1,
        }
    }

    /// Whether this build compiled it in, it exists on this platform, *and*
    /// it is implemented.
    pub fn is_available(self) -> bool {
        match self {
            Backend::VideoToolbox => {
                cfg!(all(feature = "encode-platform", target_vendor = "apple"))
            }
            Backend::Rav1e => cfg!(feature = "encode-av1"),
            Backend::MediaFoundation | Backend::MediaCodec => false,
        }
    }

    /// Whether the platform half is satisfied, ignoring features.
    fn exists_here(self) -> bool {
        match self {
            Backend::VideoToolbox => cfg!(target_vendor = "apple"),
            Backend::MediaFoundation => cfg!(windows),
            Backend::MediaCodec => cfg!(target_os = "android"),
            Backend::Rav1e => true,
        }
    }
}

impl std::fmt::Display for Backend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Backend::VideoToolbox => "VideoToolbox",
            Backend::MediaFoundation => "Media Foundation",
            Backend::MediaCodec => "MediaCodec",
            Backend::Rav1e => "rav1e",
        })
    }
}

/// Every backend, in the order [`open`] tries them.
pub const BACKENDS: [Backend; 4] = [
    Backend::VideoToolbox,
    Backend::MediaFoundation,
    Backend::MediaCodec,
    Backend::Rav1e,
];

/// The H.264 level a file declares.
///
/// The level is a promise about what decoding the stream takes, and a decoder
/// refuses a stream that promises more than it has — even one whose pictures
/// it could in fact manage. So the declared level is the compatibility
/// contract, and §15 fixes it at 4.1.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Level {
    /// High 4.1: 1080p30 or 720p60 at most. Bigger or faster pictures are
    /// scaled down to fit ([`fit_level`]) rather than declared higher.
    #[default]
    L4_1,
    /// Whatever level the picture needs — 4K included — at the cost of the
    /// older decoders that will refuse it.
    Auto,
}

/// Level 4.1's ceiling on macroblocks in one frame.
const LEVEL_4_1_FRAME: u64 = 8_192;
/// Level 4.1's ceiling on macroblocks a second.
const LEVEL_4_1_RATE: u64 = 245_760;

/// The size a picture of `width` × `height` at `frame_rate` is encoded at,
/// under `level`: unchanged if it fits, otherwise the largest even size of the
/// same shape that does.
///
/// 4.1 allows 8,192 macroblocks (16×16 pixels) a frame and 245,760 a second,
/// whichever bites first: 1920×1080 at 30 fps is 8,160 a frame and 244,800 a
/// second, just inside; at 60 fps it is twice over, and fits only at 720p.
pub fn fit_level(width: u32, height: u32, frame_rate: Rational, level: Level) -> (u32, u32) {
    let fits = |width: u32, height: u32| {
        let blocks = u64::from(width.div_ceil(16)) * u64::from(height.div_ceil(16));
        let per_second = blocks as f64 * frame_rate.as_f64().max(1.0);

        blocks <= LEVEL_4_1_FRAME
            && per_second <= LEVEL_4_1_RATE as f64
            // A frame may be no more than √(8 × 8192) = 256 macroblocks wide
            // or tall, so a panorama cannot spend the whole budget on width.
            && width.div_ceil(16) <= 256
            && height.div_ceil(16) <= 256
    };

    if level == Level::Auto || fits(width, height) {
        return (width, height);
    }

    let budget = (LEVEL_4_1_FRAME as f64)
        .min(LEVEL_4_1_RATE as f64 / frame_rate.as_f64().max(1.0))
        * 256.0;
    let mut scale = (budget / (f64::from(width) * f64::from(height))).sqrt().min(1.0);

    // Macroblock rounding can tip the first guess over; shave until it fits.
    let shaved = loop {
        let (fitted_width, fitted_height) = (
            crate::scale::even(f64::from(width) * scale),
            crate::scale::even(f64::from(height) * scale),
        );

        if fits(fitted_width, fitted_height) || fitted_width <= 16 {
            break (fitted_width, fitted_height);
        }

        scale *= 0.99;
    };

    // The shave lands just under the line — 3840×2160 comes out 1910×1074 —
    // when the familiar size one step down fits exactly. Take whichever of
    // the two is bigger.
    let standard = [1080_u32, 720, 540, 480, 360]
        .into_iter()
        .filter(|&standard| standard < height)
        .map(|standard| {
            (
                crate::scale::even(f64::from(width) * f64::from(standard) / f64::from(height)),
                standard,
            )
        })
        .find(|&(fitted_width, fitted_height)| fits(fitted_width, fitted_height));

    match standard {
        Some(standard) if standard.0 * standard.1 >= shaved.0 * shaved.1 => standard,
        _ => shaved,
    }
}

/// How to encode.
///
/// [`EncoderConfig::new`] starts from §15's spec; the fields say where it may
/// be bent.
#[derive(Clone, Debug)]
pub struct EncoderConfig {
    /// H.264 or AV1.
    pub encoding: Encoding,
    /// The picture size to write. Pictures handed to the encoder must be this
    /// size — scaling is the caller's, see [`crate::scale`].
    pub width: u32,
    /// As `width`.
    pub height: u32,
    /// Pictures a second.
    pub frame_rate: Rational,
    /// What the samples mean, written into the stream so a decoder applies
    /// the right matrix.
    pub color: ColorSpace,
    /// 0.0 to 1.0. VideoToolbox takes it as is (§15: 0.65–0.70); rav1e as a
    /// quantizer; where an encoder takes neither, it becomes a bitrate.
    pub quality: f32,
    /// A keyframe exactly this often, and never in between, so every GOP is
    /// closed and every keyframe is a place to seek to. Two seconds.
    pub keyframe_interval: Duration,
    /// Favour speed over size — what a proxy wants.
    pub realtime: bool,
    /// Whether the encoder may, must, or must not run in hardware.
    pub hardware: Hardware,
    /// The H.264 level to declare. Ignored for AV1.
    pub level: Level,
    /// Threads for a software encoder; 0 for its own choice.
    pub threads: usize,
}

impl EncoderConfig {
    /// §15's spec for a picture this size: quality 0.68, a keyframe every
    /// two seconds, High 4.1, hardware where there is any.
    pub fn new(
        encoding: Encoding,
        width: u32,
        height: u32,
        frame_rate: Rational,
        color: ColorSpace,
    ) -> Self {
        EncoderConfig {
            encoding,
            width,
            height,
            frame_rate,
            color,
            quality: 0.68,
            keyframe_interval: Duration::from_secs(2),
            realtime: false,
            hardware: Hardware::Prefer,
            level: Level::L4_1,
            threads: 0,
        }
    }

    /// The keyframe interval in frames, at least one.
    pub fn keyframe_frames(&self) -> u32 {
        (self.keyframe_interval.as_secs_f64() * self.frame_rate.as_f64())
            .round()
            .max(1.0) as u32
    }

    /// A bitrate for this quality, for an encoder that takes nothing else:
    /// 0.05 to 0.2 bits a pixel, which puts 0.68 at about 9 Mb/s for 1080p30.
    pub fn bitrate(&self) -> u32 {
        let bits_per_pixel = 0.05 + 0.15 * f64::from(self.quality.clamp(0.0, 1.0));
        let pixels = f64::from(self.width) * f64::from(self.height);

        (pixels * self.frame_rate.as_f64().max(1.0) * bits_per_pixel).min(f64::from(u32::MAX))
            as u32
    }
}

/// Which backends could encode this, here, in this build, under `hardware`.
pub fn backends_for(encoding: Encoding, hardware: Hardware) -> Vec<Backend> {
    BACKENDS
        .into_iter()
        .filter(|backend| backend.handles(encoding) && backend.is_available())
        .filter(|backend| hardware != Hardware::Require || backend.can_use_hardware())
        .collect()
}

/// Whether [`open`] would find an encoder for `encoding` under `hardware`,
/// without opening one.
pub fn is_available(encoding: Encoding, hardware: Hardware) -> bool {
    !backends_for(encoding, hardware).is_empty()
}

/// What `import` writes when not told: H.264 where the OS can encode it, AV1
/// through the bundled rav1e where it cannot. `None` in a build with neither.
pub fn default_encoding(hardware: Hardware) -> Option<Encoding> {
    [Encoding::H264, Encoding::Av1]
        .into_iter()
        .find(|encoding| is_available(*encoding, hardware))
}

/// An encoder for this configuration.
///
/// # Errors
///
/// [`Error::NoEncoder`] for an encoding vtome does not write, one nothing here
/// encodes — naming the feature or the platform — or one ruled out by
/// `hardware`; and whatever the chosen backend refuses.
pub fn open(config: &EncoderConfig) -> Result<Box<dyn Encoder>> {
    if !config.encoding.is_encodable() {
        return Err(Error::NoEncoder {
            encoding: config.encoding,
            remedy: "vtome writes H.264 (through the OS) and AV1, nothing else".to_string(),
        });
    }

    if config.width == 0 || config.height == 0 {
        return Err(Error::Encode {
            reason: format!("cannot encode a {}×{} picture", config.width, config.height),
        });
    }

    let candidates = backends_for(config.encoding, config.hardware);

    if candidates.is_empty() {
        return Err(Error::NoEncoder {
            encoding: config.encoding,
            remedy: remedy_for(config.encoding, config.hardware),
        });
    }

    let mut refusal = None;

    for backend in candidates {
        match instantiate(backend, config) {
            Some(Ok(encoder)) => return Ok(encoder),
            Some(Err(error)) => refusal = Some(error),
            None => {}
        }
    }

    Err(refusal.unwrap_or_else(|| Error::NoEncoder {
        encoding: config.encoding,
        remedy: remedy_for(config.encoding, config.hardware),
    }))
}

// `config` goes unread in a build with no encoder feature.
#[allow(unused_variables)]
fn instantiate(backend: Backend, config: &EncoderConfig) -> Option<Result<Box<dyn Encoder>>> {
    match backend {
        #[cfg(all(feature = "encode-platform", target_vendor = "apple"))]
        Backend::VideoToolbox => Some(
            crate::encode_videotoolbox::VideoToolboxEncoder::new(config)
                .map(|encoder| Box::new(encoder) as Box<dyn Encoder>),
        ),

        #[cfg(feature = "encode-av1")]
        Backend::Rav1e => Some(
            crate::encode_av1::Rav1eEncoder::new(config)
                .map(|encoder| Box::new(encoder) as Box<dyn Encoder>),
        ),

        _ => None,
    }
}

/// What a person could do about there being no encoder.
fn remedy_for(encoding: Encoding, hardware: Hardware) -> String {
    let compiled_but_ruled_out = BACKENDS
        .into_iter()
        .any(|backend| backend.handles(encoding) && backend.is_available());

    if compiled_but_ruled_out && hardware == Hardware::Require {
        return format!(
            "hardware encoding was required, and {encoding} is encoded here only in \
             software (rav1e) — allow software, or write H.264"
        );
    }

    let here: Vec<Backend> = BACKENDS
        .into_iter()
        .filter(|backend| backend.handles(encoding) && backend.exists_here())
        .collect();

    match encoding {
        Encoding::H264 if here.iter().any(|backend| backend.feature() == "encode-platform") => {
            if cfg!(feature = "encode-platform") {
                format!(
                    "{} would encode it here and is not implemented yet (planning/TODO.md \
                     §14.1); write AV1 instead",
                    here[0]
                )
            } else {
                "enable the encode-platform feature (the OS's own H.264 encoder)".to_string()
            }
        }
        Encoding::H264 => "this platform has no H.264 encoder vtome may use — H.264 is only \
                           ever the OS's, never bundled — so write AV1 instead"
            .to_string(),
        _ => "enable the encode-av1 feature (rav1e)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_4_1_holds_1080p30_and_720p60_and_scales_anything_bigger() {
        let thirty = Rational::new(30, 1);
        let sixty = Rational::new(60, 1);

        assert_eq!(fit_level(1920, 1080, thirty, Level::L4_1), (1920, 1080));
        assert_eq!(fit_level(1280, 720, sixty, Level::L4_1), (1280, 720));

        // Exactly half — not the 1910×1074 a shave alone would land on.
        assert_eq!(fit_level(3840, 2160, thirty, Level::L4_1), (1920, 1080));

        // An odd shape still fits, keeps its shape, and stays even.
        let (width, height) = fit_level(4000, 3000, thirty, Level::L4_1);
        let blocks = u64::from(width.div_ceil(16)) * u64::from(height.div_ceil(16));
        assert!(blocks <= LEVEL_4_1_FRAME, "{width}×{height}");
        assert!((f64::from(width) / f64::from(height) - 4.0 / 3.0).abs() < 0.02);
        assert!(width % 2 == 0 && height % 2 == 0);

        let (width, height) = fit_level(1920, 1080, sixty, Level::L4_1);
        let blocks = u64::from(width.div_ceil(16)) * u64::from(height.div_ceil(16));
        assert!(blocks * 60 <= LEVEL_4_1_RATE, "{width}×{height} at 60 fps");

        assert_eq!(fit_level(3840, 2160, thirty, Level::Auto), (3840, 2160));
    }

    /// The broadcast rates are ratios, and 29.97 is a hair under 30.
    #[test]
    fn ntsc_1080p_fits_level_4_1() {
        assert_eq!(
            fit_level(1920, 1080, Rational::new(30_000, 1001), Level::L4_1),
            (1920, 1080)
        );
    }

    #[test]
    fn the_defaults_are_the_spec() {
        let config = EncoderConfig::new(
            Encoding::H264,
            1920,
            1080,
            Rational::new(30, 1),
            ColorSpace::default(),
        );

        assert_eq!(config.keyframe_frames(), 60, "two seconds at 30 fps");
        assert_eq!(config.level, Level::L4_1);
        assert!((0.65..=0.70).contains(&config.quality));
        assert_eq!(config.hardware, Hardware::Prefer);

        let bitrate = config.bitrate();
        assert!((8_000_000..11_000_000).contains(&bitrate), "{bitrate}");
    }

    #[test]
    fn nothing_but_h264_and_av1_is_ever_opened() {
        let config = EncoderConfig::new(
            Encoding::Vp9,
            64,
            64,
            Rational::new(30, 1),
            ColorSpace::default(),
        );

        assert!(matches!(open(&config), Err(Error::NoEncoder { .. })));
    }

    /// rav1e is software, so requiring hardware rules AV1 out — up front.
    #[test]
    fn required_hardware_rules_out_the_software_av1_encoder() {
        assert!(backends_for(Encoding::Av1, Hardware::Require).is_empty());

        let mut config = EncoderConfig::new(
            Encoding::Av1,
            64,
            64,
            Rational::new(30, 1),
            ColorSpace::default(),
        );
        config.hardware = Hardware::Require;

        let Err(Error::NoEncoder { remedy, .. }) = open(&config) else {
            panic!("AV1 in hardware is not something vtome has");
        };
        assert!(remedy.contains("hardware") || remedy.contains("encode-av1"), "{remedy}");
    }

    #[test]
    fn h264_is_never_offered_by_a_bundled_encoder() {
        assert!(!Backend::Rav1e.handles(Encoding::H264));
        assert!(BACKENDS
            .into_iter()
            .filter(|backend| backend.handles(Encoding::H264))
            .all(|backend| backend.feature() == "encode-platform"));
    }
}
