//! What a file contains, and the compressed pieces that come out of it.
//!
//! These types are the seam between demuxing and decoding, so they live outside
//! both: a build with no demuxer can still be handed packets from somewhere
//! else — a network stream, a caller's own parser — and decode them.

use std::time::Duration;

use crate::color::ColorSpace;
use crate::identify::{Container, Encoding};

/// An exact ratio.
///
/// Frame rates are ratios and not decimals: 30000/1001 is exactly 29.97, and a
/// player that rounds it drifts by a frame every thirty-three seconds — about
/// two minutes into a feature, which is precisely long enough for nobody to
/// suspect the clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rational {
    /// Top.
    pub numerator: u32,
    /// Bottom. Never zero in a value that came from this crate.
    pub denominator: u32,
}

impl Rational {
    /// A ratio. `denominator` of zero is corrected to one, because a malformed
    /// header should not divide by zero three layers down.
    pub fn new(numerator: u32, denominator: u32) -> Self {
        Rational {
            numerator,
            denominator: denominator.max(1),
        }
    }

    /// As a decimal.
    pub fn as_f64(self) -> f64 {
        f64::from(self.numerator) / f64::from(self.denominator)
    }

    /// How long one frame lasts at this rate.
    pub fn frame_duration(self) -> Duration {
        if self.numerator == 0 {
            return Duration::ZERO;
        }

        Duration::from_secs_f64(f64::from(self.denominator) / f64::from(self.numerator))
    }

    /// The common broadcast rates, named, for messages.
    pub fn describe(self) -> String {
        let rate = self.as_f64();

        // The 1000/1001 rates read badly as decimals and everybody recognises
        // them by name.
        let name = match (self.numerator, self.denominator) {
            (24000, 1001) => Some("23.976"),
            (30000, 1001) => Some("29.97"),
            (60000, 1001) => Some("59.94"),
            _ => None,
        };

        match name {
            Some(name) => format!("{name} fps"),
            None => format!("{rate:.3} fps"),
        }
    }
}

/// How a picture should be turned before it is shown.
///
/// Phones write this rather than rotating the pixels, so a video that looks
/// sideways is usually a display matrix that went unread.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Rotation {
    /// As stored.
    #[default]
    None,
    /// A quarter turn clockwise.
    Quarter,
    /// Upside down.
    Half,
    /// A quarter turn anticlockwise.
    ThreeQuarters,
}

impl Rotation {
    /// Degrees clockwise.
    pub fn degrees(self) -> u32 {
        match self {
            Rotation::None => 0,
            Rotation::Quarter => 90,
            Rotation::Half => 180,
            Rotation::ThreeQuarters => 270,
        }
    }

    /// From degrees, rounded to the nearest quarter turn.
    pub fn from_degrees(degrees: i32) -> Self {
        match degrees.rem_euclid(360) {
            0..=44 | 315..=359 => Rotation::None,
            45..=134 => Rotation::Quarter,
            135..=224 => Rotation::Half,
            _ => Rotation::ThreeQuarters,
        }
    }

    /// Whether this rotation swaps width and height.
    pub fn is_transposed(self) -> bool {
        matches!(self, Rotation::Quarter | Rotation::ThreeQuarters)
    }
}

/// What kind of stream a track is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TrackKind {
    /// Pictures.
    Video,
    /// Sound. Reported so a caller can hand the file to an audio engine, and
    /// otherwise ignored — vtome never decodes it.
    Audio,
    /// Subtitles.
    Subtitle,
    /// Something else the container carries.
    Other,
}

/// One track of a file.
#[derive(Clone, Debug)]
pub struct TrackInfo {
    /// The container's own identifier for it.
    pub id: u32,
    /// What it carries.
    pub kind: TrackKind,
    /// The encoding, where it is one this crate knows.
    pub encoding: Option<Encoding>,
    /// The container's codec string, kept verbatim for messages about the
    /// encodings this crate does *not* know.
    pub codec_id: String,
    /// Width in pixels, before rotation.
    pub width: u32,
    /// Height in pixels, before rotation.
    pub height: u32,
    /// Frame rate, where the container states or implies one.
    pub frame_rate: Option<Rational>,
    /// Bits per sample.
    pub bit_depth: u32,
    /// What the samples mean.
    pub color: ColorSpace,
    /// How the picture should be turned.
    pub rotation: Rotation,
    /// How long the track runs.
    pub duration: Duration,
    /// The decoder configuration record — `avcC`, `hvcC`, `av1C`, or Matroska's
    /// private data. A decoder needs this *before* the first packet; without it
    /// the first seconds decode to nothing.
    pub extra_data: Vec<u8>,
}

impl TrackInfo {
    /// The dimensions as they should be displayed, with rotation applied.
    pub fn display_size(&self) -> (u32, u32) {
        if self.rotation.is_transposed() {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        }
    }

    /// Whether this crate could decode it, ignoring what is compiled in.
    pub fn is_decodable(&self) -> bool {
        matches!(
            self.encoding,
            Some(
                Encoding::H264
                    | Encoding::H265
                    | Encoding::Av1
                    | Encoding::Vp9
                    | Encoding::Vp8
                    | Encoding::Still
            )
        )
    }
}

/// Everything known about a file without decoding any of it.
#[derive(Clone, Debug)]
pub struct MediaInfo {
    /// The wrapper.
    pub container: Container,
    /// Its tracks.
    pub tracks: Vec<TrackInfo>,
    /// The longest track's duration.
    pub duration: Duration,
    /// Whether seeking is possible — false for a stream with no index.
    pub seekable: bool,
}

impl MediaInfo {
    /// The first video track, which is the one a player shows.
    pub fn video(&self) -> Option<&TrackInfo> {
        self.tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
    }

    /// Whether there is sound in here.
    ///
    /// vtome will not touch it, but a caller pairing this with an audio engine
    /// needs to know it is there.
    pub fn has_audio(&self) -> bool {
        self.tracks
            .iter()
            .any(|track| track.kind == TrackKind::Audio)
    }

    /// Whether the video track, if any, is in a format nobody charges for.
    pub fn is_royalty_free(&self) -> bool {
        self.video()
            .and_then(|track| track.encoding)
            .is_some_and(Encoding::is_royalty_free)
    }
}

/// One compressed unit out of a container: a frame, or part of one.
#[derive(Clone, Debug)]
pub struct Packet {
    /// Which track it belongs to.
    pub track_id: u32,
    /// The compressed bytes.
    pub data: Vec<u8>,
    /// When the picture inside is due.
    pub pts: Duration,
    /// When it should be decoded, which differs from `pts` wherever there are
    /// B-frames.
    pub dts: Duration,
    /// Whether decoding can start here.
    pub is_keyframe: bool,
}

impl Packet {
    /// A packet.
    pub fn new(track_id: u32, data: Vec<u8>, pts: Duration, is_keyframe: bool) -> Self {
        Packet {
            track_id,
            data,
            pts,
            dts: pts,
            is_keyframe,
        }
    }

    /// How many bytes it holds.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Whether it holds nothing. A zero-length packet is legal in some
    /// containers and means "no data this interval".
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broadcast_rates_keep_their_thousand_over_thousand_and_one() {
        let ntsc = Rational::new(30000, 1001);

        assert!((ntsc.as_f64() - 29.97002997).abs() < 1e-6);
        assert_eq!(ntsc.describe(), "29.97 fps");
        // Duration is nanosecond-resolution, so that is the tolerance to hold it to.
        assert!((ntsc.frame_duration().as_secs_f64() - 1001.0 / 30000.0).abs() < 1e-9);
    }

    #[test]
    fn a_zero_denominator_does_not_divide_by_zero() {
        let broken = Rational::new(24, 0);

        assert_eq!(broken.denominator, 1);
        assert!(broken.as_f64().is_finite());
    }

    #[test]
    fn rotation_rounds_to_quarter_turns_and_transposes() {
        assert_eq!(Rotation::from_degrees(90), Rotation::Quarter);
        assert_eq!(Rotation::from_degrees(-90), Rotation::ThreeQuarters);
        assert_eq!(Rotation::from_degrees(359), Rotation::None);
        assert_eq!(Rotation::from_degrees(450), Rotation::Quarter);

        assert!(Rotation::Quarter.is_transposed());
        assert!(!Rotation::Half.is_transposed());
    }

    /// The sideways-phone-video case.
    #[test]
    fn a_rotated_track_reports_the_size_it_should_be_shown_at() {
        let track = TrackInfo {
            id: 1,
            kind: TrackKind::Video,
            encoding: Some(Encoding::H264),
            codec_id: "avc1".to_string(),
            width: 1920,
            height: 1080,
            frame_rate: Some(Rational::new(30, 1)),
            bit_depth: 8,
            color: ColorSpace::default(),
            rotation: Rotation::Quarter,
            duration: Duration::from_secs(10),
            extra_data: Vec::new(),
        };

        assert_eq!(track.display_size(), (1080, 1920));
        assert!(track.is_decodable());
    }

    #[test]
    fn a_file_reports_its_video_track_and_whether_there_is_sound() {
        let video = TrackInfo {
            id: 1,
            kind: TrackKind::Video,
            encoding: Some(Encoding::Av1),
            codec_id: "av01".to_string(),
            width: 640,
            height: 480,
            frame_rate: None,
            bit_depth: 8,
            color: ColorSpace::default(),
            rotation: Rotation::None,
            duration: Duration::from_secs(1),
            extra_data: Vec::new(),
        };

        let audio = TrackInfo {
            id: 2,
            kind: TrackKind::Audio,
            encoding: None,
            codec_id: "opus".to_string(),
            ..video.clone()
        };

        let info = MediaInfo {
            container: Container::WebM,
            tracks: vec![audio, video.clone()],
            duration: Duration::from_secs(1),
            seekable: true,
        };

        assert_eq!(info.video().unwrap().id, video.id);
        assert!(info.has_audio(), "the audio track should be reported");
        assert!(info.is_royalty_free());
    }
}
