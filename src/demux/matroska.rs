//! Matroska and WebM.
//!
//! WebM is Matroska with the codec list cut down to the royalty-free ones, so
//! one demuxer reads both and the container type is only a label. It is also
//! the container vtome *writes*, which makes this the path that has to keep
//! working: everything else is about reading other people's files.
//!
//! Matroska states its colour metadata properly — matrix, range, bit depth —
//! where MP4 usually leaves it to be guessed. That is carried through here
//! rather than thrown away; see [`crate::color`] for why it matters.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::Duration;

use matroska_demuxer::{Frame as MatroskaFrame, MatroskaFile, TrackType};

use crate::color::{ColorSpace, Matrix, Range};
use crate::error::{Error, Result};
use crate::identify::Container;
use crate::media::{MediaInfo, Packet, Rational, Rotation, TrackInfo, TrackKind};

use super::{color_from, demux_error, scaled, video_track};

/// One Matroska or WebM file.
pub(super) struct MatroskaDemuxer {
    path: PathBuf,
    file: MatroskaFile<BufReader<File>>,
    info: MediaInfo,
    /// Ticks per second for every timestamp in the file.
    timescale: u64,
    /// Reused between packets: the crate fills a frame in place rather than
    /// allocating one, and taking its buffer keeps that true.
    scratch: MatroskaFrame,
}

impl MatroskaDemuxer {
    pub(super) fn open(path: &Path, container: Container) -> Result<Self> {
        let file = File::open(path).map_err(|error| Error::io(path, error))?;

        let file = MatroskaFile::open(BufReader::new(file))
            .map_err(|error| demux_error(&path.to_path_buf(), error))?;

        // Matroska timestamps are in units of this many nanoseconds — a
        // millisecond, almost always.
        let scale = file.info().timestamp_scale().get();
        let timescale = 1_000_000_000 / scale.max(1);

        let duration = file
            .info()
            .duration()
            .map(|ticks| Duration::from_nanos((ticks * scale as f64) as u64))
            .unwrap_or_default();

        let mut tracks = Vec::new();

        for entry in file.tracks() {
            let id = entry.track_number().get() as u32;
            let codec_id = entry.codec_id().to_string();
            let extra_data = entry.codec_private().unwrap_or_default().to_vec();

            let kind = match entry.track_type() {
                TrackType::Video => TrackKind::Video,
                TrackType::Audio => TrackKind::Audio,
                TrackType::Subtitle => TrackKind::Subtitle,
                _ => TrackKind::Other,
            };

            let Some(video) = entry.video() else {
                tracks.push(TrackInfo {
                    id,
                    kind,
                    encoding: None,
                    codec_id,
                    width: 0,
                    height: 0,
                    frame_rate: None,
                    bit_depth: 0,
                    color: ColorSpace::default(),
                    rotation: Rotation::None,
                    duration,
                    extra_data,
                });

                continue;
            };

            let width = video.pixel_width().get() as u32;
            let height = video.pixel_height().get() as u32;

            let colour = video.colour();
            let bit_depth = colour
                .and_then(|colour| colour.bits_per_channel())
                .unwrap_or(8) as u32;

            let color = color_from(
                colour
                    .and_then(|colour| colour.matrix_coefficients())
                    .and_then(matrix_from),
                colour
                    .and_then(|colour| colour.range())
                    .and_then(range_from),
                width,
                height,
            );

            // Matroska states how long a frame lasts rather than how many there
            // are a second, which is the same fact upside down.
            let frame_rate = entry
                .default_duration()
                .map(|nanoseconds| nanoseconds.get())
                .filter(|nanoseconds| *nanoseconds > 0)
                .map(|nanoseconds| {
                    Rational::new(
                        (1_000_000_000_f64 / nanoseconds as f64 * 1000.0).round() as u32,
                        1000,
                    )
                });

            tracks.push(video_track(
                id,
                codec_id,
                width,
                height,
                frame_rate,
                bit_depth,
                color,
                Rotation::None,
                duration,
                extra_data,
            ));
        }

        Ok(MatroskaDemuxer {
            path: path.to_path_buf(),
            file,
            info: MediaInfo {
                container,
                tracks,
                duration,
                seekable: true,
            },
            timescale,
            scratch: MatroskaFrame::default(),
        })
    }
}

/// Matroska's matrix coefficients, as far as they map onto ours.
///
/// The ones this crate does not name — YCoCg, constant-luminance 2020 — are
/// left to the guess rather than silently treated as something else.
fn matrix_from(coefficients: matroska_demuxer::MatrixCoefficients) -> Option<Matrix> {
    use matroska_demuxer::MatrixCoefficients;

    Some(match coefficients {
        MatrixCoefficients::Identity => Matrix::Identity,
        MatrixCoefficients::Bt709 => Matrix::Bt709,
        // 470BG and SMPTE 170M are the two spellings of BT.601.
        MatrixCoefficients::Bt470bg | MatrixCoefficients::Smpte170 => Matrix::Bt601,
        MatrixCoefficients::Bt2020Ncl => Matrix::Bt2020Ncl,
        _ => return None,
    })
}

/// Matroska's range flag.
fn range_from(range: matroska_demuxer::Range) -> Option<Range> {
    use matroska_demuxer::Range as MatroskaRange;

    match range {
        MatroskaRange::Broadcast => Some(Range::Limited),
        MatroskaRange::Full => Some(Range::Full),
        // "Defined by the transfer characteristics" is not something this crate
        // resolves; the guess is better than a coin flip.
        _ => None,
    }
}

impl super::Demuxer for MatroskaDemuxer {
    fn info(&self) -> &MediaInfo {
        &self.info
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        let more = self
            .file
            .next_frame(&mut self.scratch)
            .map_err(|error| demux_error(&self.path, error))?;

        if !more {
            return Ok(None);
        }

        let pts = scaled(self.scratch.timestamp, self.timescale);

        Ok(Some(Packet {
            track_id: self.scratch.track as u32,
            // Taking the buffer leaves an empty one behind for the next frame,
            // which is what the crate's in-place API is for.
            data: std::mem::take(&mut self.scratch.data),
            pts,
            // Matroska stores presentation time only; a decoder that needs
            // decode order derives it from the bitstream.
            dts: pts,
            // Only simple blocks carry the flag. Where the file uses block
            // groups it is absent, and treating "unknown" as a keyframe would
            // make every seek land in the middle of a GOP.
            is_keyframe: self.scratch.is_keyframe.unwrap_or(false),
        }))
    }

    fn seek(&mut self, position: Duration) -> Result<()> {
        // The crate's seek takes the file's own timestamp units and lands on
        // the cue point at or before it, which is the keyframe rule already.
        let ticks = (position.as_secs_f64() * self.timescale as f64) as u64;

        self.file
            .seek(ticks)
            .map_err(|error| demux_error(&self.path, error))
    }
}
