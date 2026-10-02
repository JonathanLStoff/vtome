//! ISO base media: `.mp4`, `.mov`, and the still-image members of the family.
//!
//! The `mp4` crate indexes every sample when the file is opened, which makes
//! this a matter of walking the index rather than parsing anything. That also
//! means seeking is arithmetic — the index says which sample is a sync sample
//! and when each one is due, so finding a keyframe costs no I/O.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::bitstream::{AvcConfig, SequenceParameterSet};
use crate::color::{ColorSpace, Matrix, Primaries, Range, Transfer};
use crate::error::{Error, Result};
use crate::identify::Container;
use crate::media::{MediaInfo, Packet, Rational, Rotation, TrackInfo, TrackKind};

use super::{color_from, demux_error, scaled, video_track};

/// Where one track has got to.
struct Cursor {
    track_id: u32,
    /// The next sample to read. Sample numbers are 1-based in the format, which
    /// is a mistake to make exactly once.
    next_sample: u32,
    /// One sample read ahead, so interleaving by timestamp does not read every
    /// sample twice.
    pending: Option<Packet>,
    /// Sync samples found so far, as `(sample number, when it is due)`.
    ///
    /// Built lazily, and never thrown away: the `mp4` crate does not expose
    /// sample timing without also handing over the bytes, so learning where the
    /// keyframes are costs a pass over the track. Doing it once per seek target
    /// and remembering the answer is the difference between a slow first seek
    /// and a slow every seek.
    keyframes: Vec<(u32, Duration)>,
    /// How far the keyframe scan has got.
    scanned_to: u32,
}

/// One MP4, with its sample index.
pub(super) struct Mp4Demuxer {
    path: PathBuf,
    reader: ::mp4::Mp4Reader<BufReader<File>>,
    info: MediaInfo,
    cursors: Vec<Cursor>,
}

impl Mp4Demuxer {
    pub(super) fn open(path: &Path, container: Container) -> Result<Self> {
        let file = File::open(path).map_err(|error| Error::io(path, error))?;
        let size = file
            .metadata()
            .map_err(|error| Error::io(path, error))?
            .len();

        let reader = ::mp4::Mp4Reader::read_header(BufReader::new(file), size)
            .map_err(|error| demux_error(&path.to_path_buf(), error))?;

        let mut tracks = Vec::new();

        // The crate hands back a HashMap, whose order is not the file's. Track
        // order is visible to callers picking "the first video track", so it is
        // sorted rather than left to chance.
        let mut ids: Vec<u32> = reader.tracks().keys().copied().collect();
        ids.sort_unstable();

        for id in ids {
            let track = &reader.tracks()[&id];

            let kind = match track.track_type() {
                Ok(::mp4::TrackType::Video) => TrackKind::Video,
                Ok(::mp4::TrackType::Audio) => TrackKind::Audio,
                Ok(::mp4::TrackType::Subtitle) => TrackKind::Subtitle,
                Err(_) => TrackKind::Other,
            };

            let duration = track.duration();
            let codec_id = track
                .box_type()
                .map(|fourcc| fourcc.to_string())
                .unwrap_or_default();

            if kind != TrackKind::Video {
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
                    extra_data: Vec::new(),
                });

                continue;
            }

            let width = u32::from(track.width());
            let height = u32::from(track.height());

            // The crate reports frame rate as a float; recovering the exact
            // ratio matters for the 1000/1001 rates, and everything else is
            // close enough to integer that rounding is safe.
            let frame_rate = rational_from(track.frame_rate());

            let extra_data = parameter_sets(track);
            let (color, bit_depth) = stated_color(&extra_data, width, height);

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

        let duration = reader.duration();
        let cursors = tracks
            .iter()
            .map(|track| Cursor {
                track_id: track.id,
                next_sample: 1,
                pending: None,
                keyframes: Vec::new(),
                scanned_to: 0,
            })
            .collect();

        Ok(Mp4Demuxer {
            path: path.to_path_buf(),
            reader,
            info: MediaInfo {
                container,
                tracks,
                duration,
                // Everything is indexed at open, so any sample is reachable.
                seekable: true,
            },
            cursors,
        })
    }
}

/// The SPS and PPS, rebuilt into an `avcC`-shaped record.
///
/// The crate exposes the parameter sets but not the box they came in, and a
/// decoder wants them before the first frame. Reassembling is cheap and keeps
/// [`TrackInfo::extra_data`](crate::media::TrackInfo::extra_data) meaning one
/// thing across containers.
fn parameter_sets(track: &::mp4::Mp4Track) -> Vec<u8> {
    let (Ok(sps), Ok(pps)) = (
        track.sequence_parameter_set(),
        track.picture_parameter_set(),
    ) else {
        return Vec::new();
    };

    AvcConfig::new(sps.to_vec(), pps.to_vec()).to_record()
}

/// The colour and bit depth the SPS states, over the guess by resolution.
///
/// MP4 says nothing about colour unless it carries a `colr` box, which most
/// files do not; the encoder wrote it into the SPS instead. Without this a
/// 640×360 proxy of a BT.709 film is guessed BT.601 and drawn with the wrong
/// matrix.
fn stated_color(extra_data: &[u8], width: u32, height: u32) -> (ColorSpace, u32) {
    let guess = ColorSpace::guess_for(width, height);

    let Some(sps) = AvcConfig::parse(extra_data)
        .ok()
        .and_then(|avc| avc.sequence_parameter_sets.into_iter().next())
        .and_then(|sps| SequenceParameterSet::parse(&sps).ok())
    else {
        return (guess, 8);
    };

    let mut color = color_from(
        sps.matrix.and_then(Matrix::from_h273),
        sps.full_range
            .map(|full| if full { Range::Full } else { Range::Limited }),
        width,
        height,
    );

    if let Some(primaries) = sps.primaries.and_then(Primaries::from_h273) {
        color.primaries = primaries;
    }

    if let Some(transfer) = sps.transfer.and_then(Transfer::from_h273) {
        color.transfer = transfer;
    }

    (color, sps.bit_depth)
}

/// Recovers an exact ratio from a decimal frame rate.
fn rational_from(rate: f64) -> Option<Rational> {
    if !rate.is_finite() || rate <= 0.0 {
        return None;
    }

    // The broadcast rates, which are the ones that must not be rounded.
    for numerator in [24_000_u32, 30_000, 60_000, 48_000, 120_000] {
        let exact = f64::from(numerator) / 1001.0;
        if (rate - exact).abs() < 0.01 {
            return Some(Rational::new(numerator, 1001));
        }
    }

    if (rate - rate.round()).abs() < 0.001 {
        return Some(Rational::new(rate.round() as u32, 1));
    }

    // Anything else: a thousandths-precision ratio, which is exact enough for
    // a rate nobody standardised.
    Some(Rational::new((rate * 1000.0).round() as u32, 1000))
}

impl Mp4Demuxer {
    /// Reads one sample of one track, turning it into a packet.
    fn read(&mut self, track_id: u32, sample_number: u32) -> Result<Option<Packet>> {
        let Some(track) = self.reader.tracks().get(&track_id) else {
            return Ok(None);
        };

        if sample_number == 0 || sample_number > track.sample_count() {
            return Ok(None);
        }

        let timescale = u64::from(track.timescale());

        let Some(sample) = self
            .reader
            .read_sample(track_id, sample_number)
            .map_err(|error| demux_error(&self.path, error))?
        else {
            return Ok(None);
        };

        // The crate's `start_time` comes from `stts`, which is the *decode*
        // timeline. Treating it as the presentation time swaps the two for
        // every file with B-frames, and a decoder reordering by PTS then
        // shows them in decode order.
        let dts = scaled(sample.start_time, timescale);

        // `ctts` carries the gap between decode and display, and it is signed:
        // a version-1 box may show a picture before its decode time so that the
        // first frame starts at zero.
        let pts = if sample.rendering_offset >= 0 {
            dts + scaled(sample.rendering_offset as u64, timescale)
        } else {
            dts.saturating_sub(scaled(sample.rendering_offset.unsigned_abs() as u64, timescale))
        };

        Ok(Some(Packet {
            track_id,
            data: sample.bytes.to_vec(),
            pts,
            dts,
            is_keyframe: sample.is_sync,
        }))
    }

    /// Fills a cursor's read-ahead slot if it is empty.
    fn fill(&mut self, index: usize) -> Result<()> {
        if self.cursors[index].pending.is_some() {
            return Ok(());
        }

        let track_id = self.cursors[index].track_id;
        let sample_number = self.cursors[index].next_sample;

        if let Some(packet) = self.read(track_id, sample_number)? {
            let cursor = &mut self.cursors[index];
            cursor.next_sample += 1;

            if packet.is_keyframe {
                cursor.keyframes.push((sample_number, packet.pts));
            }

            cursor.scanned_to = cursor.scanned_to.max(sample_number);
            cursor.pending = Some(packet);
        }

        Ok(())
    }

    /// The sample to start decoding at for a given position: the last sync
    /// sample at or before it.
    fn keyframe_before(&mut self, index: usize, position: Duration) -> Result<u32> {
        let track_id = self.cursors[index].track_id;
        let count = self
            .reader
            .tracks()
            .get(&track_id)
            .map(|track| track.sample_count())
            .unwrap_or(0);

        // Extend the scan only as far as the target, and only if it has not
        // already been passed — playing forward populates this for free.
        let mut sample_number = self.cursors[index].scanned_to + 1;

        while sample_number <= count {
            let Some(packet) = self.read(track_id, sample_number)? else {
                break;
            };

            let cursor = &mut self.cursors[index];
            cursor.scanned_to = sample_number;

            if packet.is_keyframe {
                cursor.keyframes.push((sample_number, packet.pts));
            }

            if packet.pts > position {
                break;
            }

            sample_number += 1;
        }

        Ok(self.cursors[index]
            .keyframes
            .iter()
            .filter(|(_, pts)| *pts <= position)
            .map(|(sample, _)| *sample)
            .next_back()
            .unwrap_or(1))
    }
}

impl super::Demuxer for Mp4Demuxer {
    fn info(&self) -> &MediaInfo {
        &self.info
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        // Interleave by time: whichever track's next sample is due first. A
        // file is stored roughly this way already, but "roughly" is not an
        // order a player can rely on — and a read-ahead slot per track means no
        // sample is read twice to find out.
        for index in 0..self.cursors.len() {
            self.fill(index)?;
        }

        let earliest = self
            .cursors
            .iter()
            .enumerate()
            .filter_map(|(index, cursor)| cursor.pending.as_ref().map(|packet| (index, packet.dts)))
            .min_by_key(|(_, dts)| *dts)
            .map(|(index, _)| index);

        Ok(earliest.and_then(|index| self.cursors[index].pending.take()))
    }

    fn seek(&mut self, position: Duration) -> Result<()> {
        for index in 0..self.cursors.len() {
            let landing = self.keyframe_before(index, position)?;

            self.cursors[index].next_sample = landing;
            self.cursors[index].pending = None;
        }

        Ok(())
    }
}
