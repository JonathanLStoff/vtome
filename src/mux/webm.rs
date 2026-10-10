//! AV1 into WebM: a small Matroska writer of vtome's own.
//!
//! Matroska is EBML — every element an ID, a length, and a payload — so the
//! writer is a handful of functions that make elements, and a file layout
//! chosen so the few numbers only known at the end have fixed-size slots to
//! be patched into:
//!
//! ```text
//! EBML header
//! Segment (size patched)
//!   SeekHead → Info, Tracks, Cues (Cues' position patched)
//!   Info: millisecond timestamps, Duration (patched)
//!   Tracks: one AV1 track, av1C as CodecPrivate, Colour, DefaultDuration
//!   Cluster per keyframe: Timestamp, SimpleBlocks
//!   Cues: every cluster's time and position
//! ```
//!
//! A cluster is held in memory until the next keyframe — two seconds of
//! video — and written with its size known, so no element has an unknown
//! size and a reader can skip any of them.

use std::io::{BufWriter, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{frame_duration, mux_error, require_start, Storage, VideoTrack, Written};
use crate::color::Range;
use crate::error::{Error, Result};
use crate::media::Packet;

mod id {
    pub const EBML: u32 = 0x1A45_DFA3;
    pub const EBML_VERSION: u32 = 0x4286;
    pub const EBML_READ_VERSION: u32 = 0x42F7;
    pub const EBML_MAX_ID_LENGTH: u32 = 0x42F2;
    pub const EBML_MAX_SIZE_LENGTH: u32 = 0x42F3;
    pub const DOC_TYPE: u32 = 0x4282;
    pub const DOC_TYPE_VERSION: u32 = 0x4287;
    pub const DOC_TYPE_READ_VERSION: u32 = 0x4285;

    pub const SEGMENT: u32 = 0x1853_8067;
    pub const SEEK_HEAD: u32 = 0x114D_9B74;
    pub const SEEK: u32 = 0x4DBB;
    pub const SEEK_ID: u32 = 0x53AB;
    pub const SEEK_POSITION: u32 = 0x53AC;

    pub const INFO: u32 = 0x1549_A966;
    pub const TIMESTAMP_SCALE: u32 = 0x2A_D7B1;
    pub const DURATION: u32 = 0x4489;
    pub const MUXING_APP: u32 = 0x4D80;
    pub const WRITING_APP: u32 = 0x5741;

    pub const TRACKS: u32 = 0x1654_AE6B;
    pub const TRACK_ENTRY: u32 = 0xAE;
    pub const TRACK_NUMBER: u32 = 0xD7;
    pub const TRACK_UID: u32 = 0x73C5;
    pub const TRACK_TYPE: u32 = 0x83;
    pub const FLAG_LACING: u32 = 0x9C;
    pub const CODEC_ID: u32 = 0x86;
    pub const CODEC_PRIVATE: u32 = 0x63A2;
    pub const DEFAULT_DURATION: u32 = 0x23_E383;
    pub const VIDEO: u32 = 0xE0;
    pub const PIXEL_WIDTH: u32 = 0xB0;
    pub const PIXEL_HEIGHT: u32 = 0xBA;
    pub const COLOUR: u32 = 0x55B0;
    pub const MATRIX_COEFFICIENTS: u32 = 0x55B1;
    pub const BITS_PER_CHANNEL: u32 = 0x55B2;
    pub const CHROMA_SUBSAMPLING_HORZ: u32 = 0x55B3;
    pub const CHROMA_SUBSAMPLING_VERT: u32 = 0x55B4;
    pub const RANGE: u32 = 0x55B9;
    pub const TRANSFER_CHARACTERISTICS: u32 = 0x55BA;
    pub const PRIMARIES: u32 = 0x55BB;

    pub const CLUSTER: u32 = 0x1F43_B675;
    pub const TIMESTAMP: u32 = 0xE7;
    pub const SIMPLE_BLOCK: u32 = 0xA3;

    pub const CUES: u32 = 0x1C53_BB6B;
    pub const CUE_POINT: u32 = 0xBB;
    pub const CUE_TIME: u32 = 0xB3;
    pub const CUE_TRACK_POSITIONS: u32 = 0xB7;
    pub const CUE_TRACK: u32 = 0xF7;
    pub const CUE_CLUSTER_POSITION: u32 = 0xF1;
}

/// Timestamps in milliseconds: Matroska's usual scale, and fine enough that
/// 29.97 fps rounds by under half a millisecond a frame — never accumulating,
/// because every block's time is rounded from its own timestamp.
const TIMESTAMP_SCALE: u64 = 1_000_000;

/// A block's time relative to its cluster is a signed 16-bit count of
/// milliseconds, so a cluster spans at most this long.
const CLUSTER_SPAN: u64 = 32_767;

pub(super) struct WebmMuxer<W: Storage> {
    /// The file's path, or what errors call a destination without one.
    path: PathBuf,
    file: BufWriter<W>,
    /// Run once the file is complete: `sync_all` for a file this made, nothing
    /// for a destination somebody else owns.
    settle: fn(&mut W) -> std::io::Result<()>,
    /// Bytes written so far: where the next element starts.
    position: u64,
    /// Where the Segment's payload starts; Matroska positions count from here.
    segment_data: u64,
    /// The slots patched at the end.
    segment_size_at: u64,
    cues_position_at: u64,
    duration_at: u64,
    frame: Duration,
    cluster: Option<Cluster>,
    cues: Vec<(u64, u64)>,
    samples: u64,
    first: Option<Duration>,
    last: Option<Duration>,
}

/// A cluster being filled.
struct Cluster {
    /// Its timestamp, in milliseconds.
    time: u64,
    blocks: Vec<u8>,
    starts_with_keyframe: bool,
}

impl<W: Storage> WebmMuxer<W> {
    /// `open` is called once `track` has been checked, so a configuration this
    /// cannot write does not leave an empty file behind.
    pub(super) fn create(
        open: impl FnOnce() -> Result<W>,
        path: &Path,
        settle: fn(&mut W) -> std::io::Result<()>,
        track: VideoTrack,
    ) -> Result<Self> {
        if track.config_record.is_empty() {
            return Err(mux_error("an AV1 track needs its av1C record before the first frame"));
        }

        let mut file = open()?;
        require_start(&mut file, path)?;

        let mut muxer = WebmMuxer {
            path: path.to_path_buf(),
            file: BufWriter::new(file),
            settle,
            position: 0,
            segment_data: 0,
            segment_size_at: 0,
            cues_position_at: 0,
            duration_at: 0,
            frame: frame_duration(track.frame_rate),
            cluster: None,
            cues: Vec::new(),
            samples: 0,
            first: None,
            last: None,
        };

        muxer.put(&element(
            id::EBML,
            &[
                uint(id::EBML_VERSION, 1),
                uint(id::EBML_READ_VERSION, 1),
                uint(id::EBML_MAX_ID_LENGTH, 4),
                uint(id::EBML_MAX_SIZE_LENGTH, 8),
                string(id::DOC_TYPE, "webm"),
                uint(id::DOC_TYPE_VERSION, 4),
                uint(id::DOC_TYPE_READ_VERSION, 2),
            ]
            .concat(),
        ))?;

        // Segment: its size is an eight-byte slot, patched at the end.
        muxer.put(&id_bytes(id::SEGMENT))?;
        muxer.segment_size_at = muxer.position;
        muxer.put(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF])?;
        muxer.segment_data = muxer.position;

        // SeekHead, with fixed eight-byte positions so its size never moves.
        // Info and Tracks follow it directly; Cues' position is patched.
        let seek = |target: u32, position: u64| {
            element(
                id::SEEK,
                &[
                    element(id::SEEK_ID, &id_bytes(target)),
                    element(id::SEEK_POSITION, &position.to_be_bytes()),
                ]
                .concat(),
            )
        };

        let seek_head_size = element(
            id::SEEK_HEAD,
            &[seek(id::INFO, 0), seek(id::TRACKS, 0), seek(id::CUES, 0)].concat(),
        )
        .len() as u64;

        let info = element(
            id::INFO,
            &[
                uint(id::TIMESTAMP_SCALE, TIMESTAMP_SCALE),
                string(id::MUXING_APP, "vtome"),
                string(id::WRITING_APP, "vtome"),
                element(id::DURATION, &0.0_f64.to_be_bytes()),
            ]
            .concat(),
        );

        let info_position = seek_head_size;
        let tracks_position = info_position + info.len() as u64;

        let seek_head = element(
            id::SEEK_HEAD,
            &[
                seek(id::INFO, info_position),
                seek(id::TRACKS, tracks_position),
                seek(id::CUES, 0),
            ]
            .concat(),
        );
        debug_assert_eq!(seek_head.len() as u64, seek_head_size);

        // The Cues position is the last eight bytes of the SeekHead.
        muxer.cues_position_at = muxer.position + seek_head.len() as u64 - 8;
        muxer.put(&seek_head)?;

        // The Duration float is the last eight bytes of Info.
        muxer.duration_at = muxer.position + info.len() as u64 - 8;
        muxer.put(&info)?;

        muxer.put(&tracks(&track, muxer.frame))?;

        Ok(muxer)
    }

    fn put(&mut self, bytes: &[u8]) -> Result<()> {
        self.file
            .write_all(bytes)
            .map_err(|error| Error::io(&self.path, error))?;
        self.position += bytes.len() as u64;
        Ok(())
    }

    /// Writes the cluster being filled, and notes it in the Cues if it starts
    /// at a keyframe.
    fn close_cluster(&mut self) -> Result<()> {
        let Some(cluster) = self.cluster.take() else {
            return Ok(());
        };

        if cluster.starts_with_keyframe {
            self.cues.push((cluster.time, self.position - self.segment_data));
        }

        let body = [uint(id::TIMESTAMP, cluster.time), cluster.blocks].concat();
        self.put(&element(id::CLUSTER, &body))
    }

    /// Writes `value` into the eight bytes at `at`, keeping the file where it
    /// was.
    fn patch(&mut self, at: u64, value: [u8; 8]) -> Result<()> {
        let io = |error| Error::io(&self.path, error);
        let file = self.file.get_mut();

        file.seek(SeekFrom::Start(at)).map_err(io)?;
        file.write_all(&value).map_err(io)?;
        file.seek(SeekFrom::End(0)).map_err(io)?;

        Ok(())
    }
}

impl<W: Storage> super::Muxer for WebmMuxer<W> {
    fn write(&mut self, packet: Packet) -> Result<()> {
        if self.last.is_some_and(|last| packet.pts < last) {
            return Err(mux_error(format!(
                "a packet at {:?} after one at {:?}; WebM blocks go in presentation order here",
                packet.pts,
                self.last.unwrap_or_default()
            )));
        }

        let time = milliseconds(packet.pts);

        let fresh = match &self.cluster {
            None => true,
            Some(cluster) => packet.is_keyframe || time - cluster.time > CLUSTER_SPAN,
        };

        if fresh {
            self.close_cluster()?;
            self.cluster = Some(Cluster {
                time,
                blocks: Vec::new(),
                starts_with_keyframe: packet.is_keyframe,
            });
        }

        let cluster = self.cluster.as_mut().expect("opened above");
        let relative = i16::try_from(time - cluster.time).expect("clusters are kept short");

        let mut block = Vec::with_capacity(packet.data.len() + 4);
        block.push(0x81); // track 1, as a one-byte vint
        block.extend_from_slice(&relative.to_be_bytes());
        block.push(if packet.is_keyframe { 0x80 } else { 0x00 });
        block.extend_from_slice(&packet.data);

        cluster.blocks.extend(element(id::SIMPLE_BLOCK, &block));

        self.samples += 1;
        self.first.get_or_insert(packet.pts);
        self.last = Some(packet.pts);

        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<Written> {
        self.close_cluster()?;

        let cues_position = self.position - self.segment_data;
        let points: Vec<u8> = self
            .cues
            .iter()
            .flat_map(|&(time, position)| {
                element(
                    id::CUE_POINT,
                    &[
                        uint(id::CUE_TIME, time),
                        element(
                            id::CUE_TRACK_POSITIONS,
                            &[uint(id::CUE_TRACK, 1), uint(id::CUE_CLUSTER_POSITION, position)]
                                .concat(),
                        ),
                    ]
                    .concat(),
                )
            })
            .collect();
        self.put(&element(id::CUES, &points))?;

        self.file
            .flush()
            .map_err(|error| Error::io(&self.path, error))?;

        let end = self.last.map_or(Duration::ZERO, |last| last + self.frame);
        let duration = end.saturating_sub(self.first.unwrap_or_default());

        let segment_size = self.position - self.segment_data;
        let mut slot = segment_size.to_be_bytes();
        // An eight-byte vint: the marker bit in the first byte, which the
        // size never reaches.
        slot[0] = 0x01;

        self.patch(self.segment_size_at, slot)?;
        self.patch(self.cues_position_at, cues_position.to_be_bytes())?;
        self.patch(
            self.duration_at,
            (end.as_nanos() as f64 / TIMESTAMP_SCALE as f64).to_be_bytes(),
        )?;

        let path = self.path.clone();
        let bytes = self.position;

        let mut file = self
            .file
            .into_inner()
            .map_err(|error| Error::io(&path, error.into_error()))?;
        (self.settle)(&mut file).map_err(|error| Error::io(&path, error))?;

        Ok(Written {
            samples: self.samples,
            duration,
            bytes,
        })
    }
}

/// The Tracks element: one AV1 video track.
fn tracks(track: &VideoTrack, frame: Duration) -> Vec<u8> {
    let color = track.color;

    let colour = element(
        id::COLOUR,
        &[
            uint(id::MATRIX_COEFFICIENTS, u64::from(color.matrix.h273())),
            uint(id::BITS_PER_CHANNEL, 8),
            // 4:2:0: chroma halved both ways.
            uint(id::CHROMA_SUBSAMPLING_HORZ, 1),
            uint(id::CHROMA_SUBSAMPLING_VERT, 1),
            uint(
                id::RANGE,
                match color.range {
                    Range::Limited => 1,
                    Range::Full => 2,
                },
            ),
            uint(id::TRANSFER_CHARACTERISTICS, u64::from(color.transfer.h273())),
            uint(id::PRIMARIES, u64::from(color.primaries.h273())),
        ]
        .concat(),
    );

    let video = element(
        id::VIDEO,
        &[
            uint(id::PIXEL_WIDTH, u64::from(track.width)),
            uint(id::PIXEL_HEIGHT, u64::from(track.height)),
            colour,
        ]
        .concat(),
    );

    element(
        id::TRACKS,
        &element(
            id::TRACK_ENTRY,
            &[
                uint(id::TRACK_NUMBER, 1),
                uint(id::TRACK_UID, 1),
                uint(id::TRACK_TYPE, 1),
                uint(id::FLAG_LACING, 0),
                string(id::CODEC_ID, "V_AV1"),
                element(id::CODEC_PRIVATE, &track.config_record),
                uint(id::DEFAULT_DURATION, frame.as_nanos() as u64),
                video,
            ]
            .concat(),
        ),
    )
}

fn milliseconds(time: Duration) -> u64 {
    ((time.as_nanos() + u128::from(TIMESTAMP_SCALE) / 2) / u128::from(TIMESTAMP_SCALE)) as u64
}

/// An element ID as it is written: its own bytes, length marker included.
fn id_bytes(id: u32) -> Vec<u8> {
    let bytes = id.to_be_bytes();
    let skip = bytes.iter().take_while(|&&byte| byte == 0).count().min(3);
    bytes[skip..].to_vec()
}

/// A length as the shortest EBML variable-length integer that holds it.
fn size(length: u64) -> Vec<u8> {
    // n bytes carry 7n bits, and the all-ones value is reserved for
    // "unknown", hence the minus one.
    let width = (1..=8)
        .find(|&width| length < (1_u64 << (7 * width)) - 1)
        .unwrap_or(8);

    let mut bytes = length.to_be_bytes()[8 - width..].to_vec();
    bytes[0] |= 0x80 >> (width - 1);
    bytes
}

fn element(id: u32, payload: &[u8]) -> Vec<u8> {
    let mut bytes = id_bytes(id);
    bytes.extend(size(payload.len() as u64));
    bytes.extend_from_slice(payload);
    bytes
}

/// An unsigned integer in as few bytes as hold it — at least one.
fn uint(id: u32, value: u64) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    let skip = bytes.iter().take_while(|&&byte| byte == 0).count().min(7);
    element(id, &bytes[skip..])
}

fn string(id: u32, value: &str) -> Vec<u8> {
    element(id, value.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_written_as_their_own_bytes() {
        assert_eq!(id_bytes(id::SIMPLE_BLOCK), [0xA3]);
        assert_eq!(id_bytes(id::SEEK), [0x4D, 0xBB]);
        assert_eq!(id_bytes(id::TIMESTAMP_SCALE), [0x2A, 0xD7, 0xB1]);
        assert_eq!(id_bytes(id::SEGMENT), [0x18, 0x53, 0x80, 0x67]);
    }

    #[test]
    fn sizes_take_the_fewest_bytes_and_never_spell_unknown() {
        assert_eq!(size(0), [0x80]);
        assert_eq!(size(126), [0xFE]);
        // 127 in one byte would be all ones — "unknown" — so it takes two.
        assert_eq!(size(127), [0x40, 0x7F]);
        assert_eq!(size(300), [0x41, 0x2C]);
    }

    #[test]
    fn integers_drop_their_leading_zeros_but_keep_one_byte() {
        assert_eq!(uint(id::TRACK_NUMBER, 1), [0xD7, 0x81, 0x01]);
        assert_eq!(uint(id::TRACK_NUMBER, 0), [0xD7, 0x81, 0x00]);
        assert_eq!(uint(id::CUE_TIME, 0x1234), [0xB3, 0x82, 0x12, 0x34]);
    }
}
