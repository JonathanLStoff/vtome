//! Taking a container apart, without decoding anything inside it.
//!
//! Two containers, both pure Rust: ISO base media (`.mp4`, `.mov`, and the
//! still-image members of the family) and Matroska/WebM. That is not a
//! limitation to apologise for — between them they carry everything anyone
//! ships today, and the alternative is linking a demuxer library the size of
//! the rest of this crate several times over.
//!
//! ```no_run
//! let mut media = vtome::open_media("clip.webm")?;
//! let track = media.info().video().expect("no video in there").id;
//!
//! while let Some(packet) = media.next_packet()? {
//!     if packet.track_id == track {
//!         // ...into a decoder. The bytes are still compressed here.
//!     }
//! }
//! # Ok::<(), vtome::Error>(())
//! ```
//!
//! # What a demuxer here does *not* do
//!
//! It does not decode, and it does not touch audio beyond reporting that it is
//! there. Audio packets come out of [`Demuxer::next_packet`] like any other, so
//! a caller pairing this with an audio engine can route them; vtome itself
//! never looks inside one.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::color::{ColorSpace, Matrix, Range};
use crate::error::{Error, Result};
use crate::identify::{identify_path, Container, Encoding};
use crate::media::{MediaInfo, Packet, Rational, Rotation, TrackInfo, TrackKind};

mod matroska;
mod mp4;

/// A container that has been opened and can be read through.
pub trait Demuxer: Send {
    /// What is in the file.
    fn info(&self) -> &MediaInfo;

    /// The next packet, in the order the file stores them, or `None` at the
    /// end.
    ///
    /// Packets come out interleaved across tracks, because that is how they are
    /// stored — a caller that wants one track filters on
    /// [`Packet::track_id`](crate::media::Packet::track_id).
    fn next_packet(&mut self) -> Result<Option<Packet>>;

    /// Moves to the last keyframe at or before `position`.
    ///
    /// Keyframe rather than exact frame, always: decoding cannot begin
    /// anywhere else. A player that wants the exact frame decodes forward from
    /// here and discards, which is a policy decision and so is not made here.
    fn seek(&mut self, position: Duration) -> Result<()>;
}

/// Opens a media file, whatever container it turns out to be.
///
/// Identification is by content — see [`crate::identify`] — so a Matroska file
/// named `.mp4` opens as Matroska rather than failing.
///
/// # Errors
///
/// [`Error::Io`] if it cannot be read, [`Error::UnknownContainer`] if nothing
/// recognises it, [`Error::Unsupported`] for a container vtome identifies but
/// does not open, [`Error::Demux`] if it is malformed.
pub fn open(path: impl AsRef<Path>) -> Result<Box<dyn Demuxer>> {
    let path = path.as_ref();
    let container = identify_path(path)?;

    match container {
        Container::Mp4 | Container::Avif | Container::Heif => {
            Ok(Box::new(mp4::Mp4Demuxer::open(path, container)?))
        }

        Container::Matroska | Container::WebM => {
            Ok(Box::new(matroska::MatroskaDemuxer::open(path, container)?))
        }

        _ if container.is_image() => Err(Error::unsupported(format!(
            "{} is a {container}, which is a still image — use the `image` feature \
             and `vtome::load_image` rather than a demuxer",
            path.display()
        ))),

        _ => Err(Error::unsupported(format!(
            "{} is a {container}, which vtome identifies but does not open. \
             Transcode it to WebM or MP4 first",
            path.display()
        ))),
    }
}

/// Turns a container-scale timestamp into a duration.
///
/// Timescales are ticks per second and are commonly 90000 or 1000000000, so
/// this multiplies before dividing — in `u128`, because a two-hour film at a
/// nanosecond timescale overflows `u64` in the intermediate.
fn scaled(ticks: u64, per_second: u64) -> Duration {
    if per_second == 0 {
        return Duration::ZERO;
    }

    let nanoseconds = u128::from(ticks) * 1_000_000_000 / u128::from(per_second);

    Duration::from_nanos(nanoseconds.min(u128::from(u64::MAX)) as u64)
}

/// The colour space a container's own metadata describes, falling back to the
/// guess by resolution where it says nothing — which is most files.
fn color_from(matrix: Option<Matrix>, range: Option<Range>, width: u32, height: u32) -> ColorSpace {
    let mut color = ColorSpace::guess_for(width, height);

    if let Some(matrix) = matrix {
        color.matrix = matrix;
    }

    if let Some(range) = range {
        color.range = range;
    }

    color
}

/// Shared construction of a video track's description.
#[allow(clippy::too_many_arguments)]
fn video_track(
    id: u32,
    codec_id: String,
    width: u32,
    height: u32,
    frame_rate: Option<Rational>,
    bit_depth: u32,
    color: ColorSpace,
    rotation: Rotation,
    duration: Duration,
    extra_data: Vec<u8>,
) -> TrackInfo {
    TrackInfo {
        id,
        kind: TrackKind::Video,
        encoding: Encoding::from_codec_id(&codec_id),
        codec_id,
        width,
        height,
        frame_rate,
        bit_depth,
        color,
        rotation,
        duration,
        extra_data,
    }
}

/// A path in an error, without repeating the formatting at every call site.
fn demux_error(path: &PathBuf, reason: impl std::fmt::Display) -> Error {
    Error::demux(path, reason.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timescales_convert_without_overflowing() {
        assert_eq!(scaled(90_000, 90_000), Duration::from_secs(1));
        assert_eq!(scaled(1_500, 1_000), Duration::from_millis(1_500));

        // Two hours at a nanosecond timescale, which overflows u64 if the
        // multiply happens before the divide in 64 bits.
        let two_hours = 7_200 * 1_000_000_000_u64;
        assert_eq!(scaled(two_hours, 1_000_000_000), Duration::from_secs(7_200));
    }

    #[test]
    fn a_zero_timescale_does_not_divide_by_zero() {
        assert_eq!(scaled(1_000, 0), Duration::ZERO);
    }

    #[test]
    fn container_metadata_wins_over_the_guess() {
        // 4K, so the guess would be BT.2020.
        let guessed = color_from(None, None, 3840, 2160);
        assert_eq!(guessed.matrix, Matrix::Bt2020Ncl);

        let stated = color_from(Some(Matrix::Bt709), Some(Range::Full), 3840, 2160);
        assert_eq!(stated.matrix, Matrix::Bt709);
        assert_eq!(stated.range, Range::Full);
    }

    #[test]
    fn a_still_image_is_pointed_at_the_image_loader_rather_than_a_demuxer() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("picture.png");
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.extend_from_slice(&[0; 32]);
        std::fs::write(&path, bytes).unwrap();

        let Err(Error::Unsupported { what }) = open(&path) else {
            panic!("a PNG is not a container to demux");
        };

        assert!(what.contains("load_image"), "{what}");
    }

    #[test]
    fn a_container_we_identify_but_do_not_open_says_so() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("old.avi");
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&[0; 4]);
        bytes.extend_from_slice(b"AVI ");
        bytes.extend_from_slice(&[0; 32]);
        std::fs::write(&path, bytes).unwrap();

        let Err(Error::Unsupported { what }) = open(&path) else {
            panic!("AVI is identified and not opened");
        };

        assert!(what.contains("Transcode"), "{what}");
    }

    #[test]
    fn a_file_that_is_not_media_at_all_is_reported_as_such() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("notes.txt");
        std::fs::write(&path, b"just some text").unwrap();

        assert!(matches!(open(&path), Err(Error::UnknownContainer { .. })));
    }
}
