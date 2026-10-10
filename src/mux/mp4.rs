//! H.264 into MP4, through the `mp4` crate's writer, finished faststart.

use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{frame_duration, mux_error, require_start, Storage, VideoTrack, Written};
use crate::bitstream::AvcConfig;
use crate::error::{Error, Result};
use crate::media::Packet;

/// MP4's own clock for the whole file; tracks keep their own.
const MOVIE_TIMESCALE: u32 = 1000;

pub(super) struct Mp4Muxer<W: Storage> {
    /// The file's path, or what errors call a destination without one.
    path: PathBuf,
    /// Whether the index is moved in `writer` itself rather than by rewriting
    /// the file at `path` — the difference between a destination somebody else
    /// owns and one this made.
    in_place: bool,
    writer: ::mp4::Mp4Writer<BufWriter<W>>,
    /// Ticks a second on the track's clock: one that divides the frame
    /// duration exactly, so 29.97 stays 29.97.
    timescale: u32,
    frame: Duration,
    /// Held until the next one says how long it lasts.
    pending: Option<Packet>,
    samples: u64,
    first: Option<Duration>,
    end: Duration,
}

impl<W: Storage> Mp4Muxer<W> {
    /// `open` is called once `track` has been checked, so a configuration this
    /// cannot write does not leave an empty file behind.
    pub(super) fn create(
        open: impl FnOnce() -> Result<W>,
        path: &Path,
        in_place: bool,
        track: VideoTrack,
    ) -> Result<Self> {
        let avc = AvcConfig::parse(&track.config_record)?;
        let (Some(sps), Some(pps)) = (
            avc.sequence_parameter_sets.first(),
            avc.picture_parameter_sets.first(),
        ) else {
            return Err(mux_error("the avcC record has no SPS or no PPS to write"));
        };

        if avc.nal_length_size != 4 {
            return Err(mux_error(
                "the mp4 writer stores four-byte NAL lengths, and these samples use another size",
            ));
        }

        let (Ok(width), Ok(height)) = (u16::try_from(track.width), u16::try_from(track.height))
        else {
            return Err(mux_error(format!(
                "{}×{} is larger than an MP4 track header holds",
                track.width, track.height
            )));
        };

        let mut file = open()?;
        require_start(&mut file, path)?;

        let config = ::mp4::Mp4Config {
            major_brand: brand("isom"),
            minor_version: 512,
            compatible_brands: vec![brand("isom"), brand("iso2"), brand("avc1"), brand("mp41")],
            timescale: MOVIE_TIMESCALE,
        };

        let mut writer = ::mp4::Mp4Writer::write_start(BufWriter::new(file), &config)
            .map_err(|error| mp4_error(path, error))?;

        // 30000/1001 → 30000 ticks a second, 1001 a frame. A whole-number rate
        // gets a thousand ticks a frame, room for a timestamp that wanders.
        let rate = track.frame_rate;
        let timescale = if rate.numerator >= 1000 {
            rate.numerator
        } else {
            rate.numerator.max(1).saturating_mul(1000)
        };

        writer
            .add_track(&::mp4::TrackConfig {
                track_type: ::mp4::TrackType::Video,
                timescale,
                language: "und".to_string(),
                media_conf: ::mp4::MediaConfig::AvcConfig(::mp4::AvcConfig {
                    width,
                    height,
                    seq_param_set: sps.clone(),
                    pic_param_set: pps.clone(),
                }),
            })
            .map_err(|error| mp4_error(path, error))?;

        Ok(Mp4Muxer {
            path: path.to_path_buf(),
            in_place,
            writer,
            timescale,
            frame: frame_duration(rate),
            pending: None,
            samples: 0,
            first: None,
            end: Duration::ZERO,
        })
    }

    fn ticks(&self, time: Duration) -> u64 {
        ((time.as_nanos() * u128::from(self.timescale) + 500_000_000) / 1_000_000_000) as u64
    }

    /// Writes `packet`, lasting until `next` starts.
    fn put(&mut self, packet: Packet, next: Duration) -> Result<()> {
        let start = self.ticks(packet.dts);
        let duration = self.ticks(next).saturating_sub(start).max(1);
        let offset = i64::try_from(self.ticks(packet.pts)).unwrap_or(i64::MAX)
            - i64::try_from(start).unwrap_or(i64::MAX);

        let sample = ::mp4::Mp4Sample {
            start_time: start,
            duration: u32::try_from(duration).map_err(|_| mux_error("a sample lasting days"))?,
            rendering_offset: i32::try_from(offset).unwrap_or(0),
            is_sync: packet.is_keyframe,
            bytes: ::mp4::Bytes::from(packet.data),
        };

        self.writer
            .write_sample(1, &sample)
            .map_err(|error| mp4_error(&self.path, error))?;

        self.samples += 1;
        self.first.get_or_insert(packet.pts);
        self.end = self.end.max(packet.pts + (next.saturating_sub(packet.dts)));

        Ok(())
    }
}

impl<W: Storage> super::Muxer for Mp4Muxer<W> {
    fn write(&mut self, packet: Packet) -> Result<()> {
        if let Some(previous) = self.pending.take() {
            if packet.dts < previous.dts {
                return Err(mux_error(format!(
                    "a packet decoded at {:?} after one at {:?}",
                    packet.dts, previous.dts
                )));
            }

            let next = packet.dts;
            self.put(previous, next)?;
        }

        self.pending = Some(packet);
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<Written> {
        if let Some(last) = self.pending.take() {
            let next = last.dts + self.frame;
            self.put(last, next)?;
        }

        self.writer
            .write_end()
            .map_err(|error| mp4_error(&self.path, error))?;

        let path = self.path.clone();
        let samples = self.samples;
        let duration = self.end.saturating_sub(self.first.unwrap_or_default());

        // Flushes what the buffer holds, and hands back what it was writing to.
        let mut inner = self
            .writer
            .into_writer()
            .into_inner()
            .map_err(|error| Error::io(&path, error.into_error()))?;

        let bytes = if self.in_place {
            faststart_in_place(&mut inner, &path)?
        } else {
            // A file this made: closed first, since the rewrite opens it by
            // path and renames over it.
            drop(inner);
            faststart(&path)?;

            std::fs::metadata(&path)
                .map_err(|error| Error::io(&path, error))?
                .len()
        };

        Ok(Written {
            samples,
            duration,
            bytes,
        })
    }
}

fn brand(text: &str) -> ::mp4::FourCC {
    text.parse().expect("a four-character brand")
}

fn mp4_error(path: &Path, error: ::mp4::Error) -> Error {
    match error {
        ::mp4::Error::IoError(error) => Error::io(path, error),
        other => mux_error(format!("{}: {other}", path.display())),
    }
}

/// One top-level box: where it starts and how long it is, header included.
#[derive(Clone, Copy, Debug)]
struct TopBox {
    kind: [u8; 4],
    offset: u64,
    size: u64,
}

/// The top-level boxes of the file, in order.
fn top_boxes(file: &mut (impl Read + Seek), length: u64) -> std::io::Result<Vec<TopBox>> {
    let mut boxes = Vec::new();
    let mut offset = 0;

    while offset + 8 <= length {
        file.seek(SeekFrom::Start(offset))?;

        let mut header = [0_u8; 8];
        file.read_exact(&mut header)?;

        let mut size = u64::from(u32::from_be_bytes([header[0], header[1], header[2], header[3]]));
        let kind = [header[4], header[5], header[6], header[7]];

        if size == 1 {
            let mut large = [0_u8; 8];
            file.read_exact(&mut large)?;
            size = u64::from_be_bytes(large);
        } else if size == 0 {
            size = length - offset;
        }

        if size < 8 || offset + size > length {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "the '{}' box at {offset} claims {size} bytes",
                    String::from_utf8_lossy(&kind)
                ),
            ));
        }

        boxes.push(TopBox { kind, offset, size });
        offset += size;
    }

    Ok(boxes)
}

/// What moving the index to the front comes to, worked out before anything
/// is changed.
struct Reorder {
    boxes: Vec<TopBox>,
    moov: TopBox,
    mdat: TopBox,
    /// The index as it will be written: every chunk offset already moved down
    /// by this box's own length.
    index: Vec<u8>,
}

/// Works out the reorder, or `None` if the index is already first.
fn reorder(file: &mut (impl Read + Seek), length: u64, path: &Path) -> Result<Option<Reorder>> {
    let io = |error| Error::io(path, error);

    let boxes = top_boxes(file, length).map_err(io)?;

    let find = |kind: &[u8; 4]| boxes.iter().find(|entry| &entry.kind == kind).copied();

    let (Some(moov), Some(mdat)) = (find(b"moov"), find(b"mdat")) else {
        return Err(mux_error(format!(
            "{}: no moov or no mdat to put in order",
            path.display()
        )));
    };

    if moov.offset < mdat.offset {
        return Ok(None);
    }

    let mut index = vec![0_u8; moov.size as usize];
    file.seek(SeekFrom::Start(moov.offset)).map_err(io)?;
    file.read_exact(&mut index).map_err(io)?;

    // The index's new length decides the shift, and the shift decides whether
    // 32-bit offsets still hold — so measure with 32 first, and widen only if
    // the shifted offsets do not fit.
    let narrow = shift_offsets(&index, 0, false)?;
    let index = match shift_offsets(&index, narrow.len() as u64, false) {
        Ok(shifted) => shifted,
        Err(_) => {
            let wide = shift_offsets(&index, 0, true)?;
            shift_offsets(&index, wide.len() as u64, true)?
        }
    };

    Ok(Some(Reorder {
        boxes,
        moov,
        mdat,
        index,
    }))
}

/// Moves `moov` in front of `mdat`, so a player has the index before the
/// media rather than after it.
///
/// The `mp4` crate writes the index last because only then is it known. This
/// writes a new file — everything before `mdat`, then the index with every
/// chunk offset moved down by its own length, then the rest — and renames it
/// over the old one, copying the media through in blocks rather than holding
/// it. An offset pushed past four gigabytes turns its `stco` into a `co64`.
pub(crate) fn faststart(path: &Path) -> Result<()> {
    let io = |error| Error::io(path, error);

    let mut file = File::open(path).map_err(io)?;
    let length = file.metadata().map_err(io)?.len();

    let Some(Reorder {
        boxes, mdat, index, ..
    }) = reorder(&mut file, length, path)?
    else {
        return Ok(());
    };

    let temporary = path.with_extension("faststart-partial");
    let result = (|| -> std::io::Result<()> {
        let mut output = BufWriter::new(File::create(&temporary)?);

        for entry in &boxes {
            if entry.kind == *b"moov" {
                continue;
            }

            if entry.offset == mdat.offset {
                output.write_all(&index)?;
            }

            file.seek(SeekFrom::Start(entry.offset))?;
            std::io::copy(&mut (&mut file).take(entry.size), &mut output)?;
        }

        output.flush()?;
        output.into_inner().map_err(|error| error.into_error())?.sync_all()
    })();

    if let Err(error) = result {
        let _ = std::fs::remove_file(&temporary);
        return Err(Error::io(&temporary, error));
    }

    drop(file);
    std::fs::rename(&temporary, path).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        Error::io(path, error)
    })
}

/// How much of the media is moved at a time.
const MOVE_BLOCK: usize = 1 << 20;

/// [`faststart`] for a destination that is not a path: the same file, the same
/// result, rearranged where it lies.
///
/// A file somebody else owns cannot be renamed over, so the media is shifted
/// forward by the index's length — last block first, so nothing is overwritten
/// before it has been moved — and the index is written into the gap. Needs the
/// index to be the last box, which is how the `mp4` crate leaves it, and
/// needs no second copy of the file anywhere.
///
/// Returns the file's length.
pub(crate) fn faststart_in_place(file: &mut impl Storage, path: &Path) -> Result<u64> {
    let io = |error| Error::io(path, error);

    let length = file.seek(SeekFrom::End(0)).map_err(io)?;

    let Some(Reorder {
        boxes, moov, mdat, index,
    }) = reorder(file, length, path)?
    else {
        return Ok(length);
    };

    if boxes.last().is_none_or(|last| last.offset != moov.offset) {
        return Err(mux_error(format!(
            "{}: the index is not the last box, and a file cannot be rearranged in place around it",
            path.display()
        )));
    }

    let gap = index.len() as u64;

    // A shorter index would leave the tail of the old one behind, and a
    // destination that is only a writer cannot be cut short.
    if gap < moov.size {
        return Err(mux_error(format!(
            "{}: the rewritten index is shorter than the one it replaces",
            path.display()
        )));
    }

    let mut block = vec![0_u8; MOVE_BLOCK];
    let mut remaining = moov.offset - mdat.offset;

    while remaining > 0 {
        let take = remaining.min(MOVE_BLOCK as u64) as usize;
        let from = mdat.offset + remaining - take as u64;

        file.seek(SeekFrom::Start(from)).map_err(io)?;
        file.read_exact(&mut block[..take]).map_err(io)?;
        file.seek(SeekFrom::Start(from + gap)).map_err(io)?;
        file.write_all(&block[..take]).map_err(io)?;

        remaining -= take as u64;
    }

    file.seek(SeekFrom::Start(mdat.offset)).map_err(io)?;
    file.write_all(&index).map_err(io)?;

    let end = moov.offset + gap;
    file.seek(SeekFrom::Start(end)).map_err(io)?;
    file.flush().map_err(io)?;

    Ok(end)
}

/// Box types whose payload is boxes, on the way from `moov` to a chunk table.
const CONTAINERS: [&[u8; 4]; 5] = [b"moov", b"trak", b"mdia", b"minf", b"stbl"];

/// `moov` (header included) with every chunk offset moved down by `shift`, as
/// 64-bit `co64` tables if `wide`.
///
/// # Errors
///
/// [`Error::Encode`] for a box that runs past its parent, and for a 32-bit
/// offset that no longer fits — the caller's cue to widen.
fn shift_offsets(data: &[u8], shift: u64, wide: bool) -> Result<Vec<u8>> {
    let malformed = |what: &str| mux_error(format!("the moov box is malformed: {what}"));

    if data.len() < 8 {
        return Err(malformed("a box shorter than its header"));
    }

    let mut kind = [0_u8; 4];
    kind.copy_from_slice(&data[4..8]);

    let declared = u64::from(u32::from_be_bytes([data[0], data[1], data[2], data[3]]));
    let (header, size) = if declared == 1 {
        if data.len() < 16 {
            return Err(malformed("a large box shorter than its header"));
        }
        let mut large = [0_u8; 8];
        large.copy_from_slice(&data[8..16]);
        (16, u64::from_be_bytes(large))
    } else if declared == 0 {
        (8, data.len() as u64)
    } else {
        (8, declared)
    };

    if size as usize > data.len() || (size as usize) < header {
        return Err(malformed("a box runs past its parent"));
    }

    let payload = &data[header..size as usize];

    let body = if CONTAINERS.contains(&&kind) {
        let mut body = Vec::with_capacity(payload.len());
        let mut cursor = 0;

        while cursor + 8 <= payload.len() {
            let child_size =
                u32::from_be_bytes([payload[cursor], payload[cursor + 1], payload[cursor + 2], payload[cursor + 3]])
                    as usize;
            let child_size = match child_size {
                0 => payload.len() - cursor,
                1 if cursor + 16 <= payload.len() => {
                    let mut large = [0_u8; 8];
                    large.copy_from_slice(&payload[cursor + 8..cursor + 16]);
                    u64::from_be_bytes(large) as usize
                }
                size => size,
            };

            if child_size < 8 || cursor + child_size > payload.len() {
                return Err(malformed("a child box runs past its parent"));
            }

            body.extend(shift_offsets(&payload[cursor..cursor + child_size], shift, wide)?);
            cursor += child_size;
        }

        body
    } else if &kind == b"stco" || &kind == b"co64" {
        let entry = if &kind == b"stco" { 4 } else { 8 };

        if payload.len() < 8 {
            return Err(malformed("a chunk table shorter than its count"));
        }

        let count = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]) as usize;
        if payload.len() < 8 + count * entry {
            return Err(malformed("a chunk table shorter than its entries"));
        }

        let wide = wide || &kind == b"co64";
        kind = if wide { *b"co64" } else { *b"stco" };

        let mut body = payload[..8].to_vec();
        for chunk in payload[8..8 + count * entry].chunks_exact(entry) {
            let old = if entry == 4 {
                u64::from(u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            } else {
                let mut bytes = [0_u8; 8];
                bytes.copy_from_slice(chunk);
                u64::from_be_bytes(bytes)
            };

            let new = old + shift;

            if wide {
                body.extend_from_slice(&new.to_be_bytes());
            } else {
                let narrow = u32::try_from(new)
                    .map_err(|_| mux_error("a chunk offset no longer fits in 32 bits"))?;
                body.extend_from_slice(&narrow.to_be_bytes());
            }
        }

        body
    } else {
        payload.to_vec()
    };

    let total = 8 + body.len() as u64;
    let mut output = Vec::with_capacity(total as usize + 8);

    if let Ok(total) = u32::try_from(total) {
        output.extend_from_slice(&total.to_be_bytes());
        output.extend_from_slice(&kind);
    } else {
        output.extend_from_slice(&1_u32.to_be_bytes());
        output.extend_from_slice(&kind);
        output.extend_from_slice(&(total + 8).to_be_bytes());
    }

    output.extend(body);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut data = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        data.extend_from_slice(kind);
        data.extend_from_slice(payload);
        data
    }

    fn stco(offsets: &[u32]) -> Vec<u8> {
        let mut payload = vec![0, 0, 0, 0];
        payload.extend_from_slice(&(offsets.len() as u32).to_be_bytes());
        for offset in offsets {
            payload.extend_from_slice(&offset.to_be_bytes());
        }
        boxed(b"stco", &payload)
    }

    fn moov(table: Vec<u8>) -> Vec<u8> {
        let stbl = boxed(b"stbl", &table);
        let minf = boxed(b"minf", &stbl);
        let mdia = boxed(b"mdia", &minf);
        let trak = boxed(b"trak", &mdia);
        boxed(b"moov", &[boxed(b"mvhd", &[7; 12]), trak].concat())
    }

    fn offsets_in(moov: &[u8]) -> (Vec<u64>, bool) {
        let at = moov.windows(4).position(|w| w == b"stco" || w == b"co64").unwrap();
        let wide = &moov[at..at + 4] == b"co64";
        let count = u32::from_be_bytes(moov[at + 8..at + 12].try_into().unwrap()) as usize;
        let entry = if wide { 8 } else { 4 };

        let offsets = (0..count)
            .map(|index| {
                let start = at + 12 + index * entry;
                let bytes = &moov[start..start + entry];
                if wide {
                    u64::from_be_bytes(bytes.try_into().unwrap())
                } else {
                    u64::from(u32::from_be_bytes(bytes.try_into().unwrap()))
                }
            })
            .collect();

        (offsets, wide)
    }

    #[test]
    fn every_chunk_offset_moves_by_the_shift_and_nothing_else_changes() {
        let original = moov(stco(&[40, 1_000, 52_000]));
        let shifted = shift_offsets(&original, 500, false).unwrap();

        assert_eq!(shifted.len(), original.len());
        assert_eq!(offsets_in(&shifted), (vec![540, 1_500, 52_500], false));
        // The header box beside the track is untouched.
        assert_eq!(&shifted[16..28], &[7; 12]);
    }

    /// Past four gigabytes a 32-bit table cannot hold the offset; widening
    /// makes it a co64, and every parent's size grows with it.
    #[test]
    fn an_offset_pushed_past_32_bits_needs_a_wide_table() {
        let original = moov(stco(&[u32::MAX - 10, 100]));

        assert!(shift_offsets(&original, 500, false).is_err());

        let wide = shift_offsets(&original, 500, true).unwrap();
        assert_eq!(wide.len(), original.len() + 8, "two entries, four bytes more each");
        assert_eq!(
            offsets_in(&wide),
            (vec![u64::from(u32::MAX) + 490, 600], true)
        );
        assert_eq!(
            u32::from_be_bytes(wide[0..4].try_into().unwrap()) as usize,
            wide.len(),
            "moov's own size follows"
        );
    }

    #[test]
    fn a_file_is_rewritten_with_its_index_first() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("clip.mp4");

        let ftyp = boxed(b"ftyp", b"isom\0\0\x02\0isom");
        let mdat = boxed(b"mdat", b"0123456789");
        // The one chunk starts at the first byte of mdat's payload.
        let chunk = (ftyp.len() + 8) as u32;
        let index = moov(stco(&[chunk]));

        std::fs::write(&path, [ftyp.clone(), mdat.clone(), index.clone()].concat()).unwrap();
        faststart(&path).unwrap();

        let rewritten = std::fs::read(&path).unwrap();
        assert_eq!(&rewritten[..ftyp.len()], &ftyp[..]);
        assert_eq!(&rewritten[ftyp.len() + 4..ftyp.len() + 8], b"moov");

        let (offsets, _) = offsets_in(&rewritten[ftyp.len()..]);
        let start = offsets[0] as usize;
        assert_eq!(&rewritten[start..start + 10], b"0123456789", "the offset still lands on the media");

        // Already in order: left alone.
        faststart(&path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), rewritten);
    }

    /// A file as the `mp4` crate leaves it: `ftyp`, `mdat`, then `moov` last.
    fn index_last(mdat_len: usize, offsets: &[u32]) -> Vec<u8> {
        let media: Vec<u8> = (0..mdat_len).map(|index| (index % 251) as u8).collect();

        [
            boxed(b"ftyp", b"isom\0\0\x02\0isomiso2avc1mp41"),
            boxed(b"mdat", &media),
            moov(stco(offsets)),
        ]
        .concat()
    }

    fn boxes_of(bytes: &[u8]) -> Vec<[u8; 4]> {
        top_boxes(&mut std::io::Cursor::new(bytes), bytes.len() as u64)
            .unwrap()
            .into_iter()
            .map(|entry| entry.kind)
            .collect()
    }

    /// The rearrangement in place must give the bytes the rewrite-and-rename
    /// gives. `mdat_len` is more than a block, so the media is moved in several
    /// pieces and the last-block-first order is exercised.
    #[test]
    fn rearranging_in_place_gives_what_rewriting_the_file_gives() {
        let original = index_last(MOVE_BLOCK * 5 / 2 + 123, &[40, 1_000, 52_000]);

        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("rewritten.mp4");
        std::fs::write(&path, &original).unwrap();
        faststart(&path).unwrap();
        let rewritten = std::fs::read(&path).unwrap();

        let mut stream = std::io::Cursor::new(original.clone());
        let length = faststart_in_place(&mut stream, Path::new("stream")).unwrap();

        assert_eq!(stream.get_ref(), &rewritten);
        assert_eq!(length, rewritten.len() as u64);
        assert_eq!(stream.position(), length, "left at the end, for whoever writes next");
        assert_eq!(
            boxes_of(stream.get_ref()),
            [*b"ftyp", *b"moov", *b"mdat"],
            "the index is first"
        );
    }

    /// A table that has to widen makes the index longer, so the file grows: the
    /// in-place version has to write past the old end rather than stop there.
    #[test]
    fn a_widened_index_grows_the_file_in_place() {
        let original = index_last(3_000, &[u32::MAX - 10, 100]);

        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("rewritten.mp4");
        std::fs::write(&path, &original).unwrap();
        faststart(&path).unwrap();
        let rewritten = std::fs::read(&path).unwrap();

        let mut stream = std::io::Cursor::new(original.clone());
        let length = faststart_in_place(&mut stream, Path::new("stream")).unwrap();

        assert_eq!(rewritten.len(), original.len() + 8);
        assert_eq!(stream.get_ref(), &rewritten);
        assert_eq!(length, rewritten.len() as u64);
    }

    #[test]
    fn a_file_already_in_order_is_left_alone_in_place() {
        let original = [
            boxed(b"ftyp", b"isom\0\0\x02\0"),
            moov(stco(&[40])),
            boxed(b"mdat", &[1, 2, 3]),
        ]
        .concat();

        let mut stream = std::io::Cursor::new(original.clone());
        let length = faststart_in_place(&mut stream, Path::new("stream")).unwrap();

        assert_eq!(stream.into_inner(), original);
        assert_eq!(length, original.len() as u64);
    }

    #[test]
    fn an_index_that_is_not_last_is_refused_in_place_rather_than_corrupted() {
        let original = [
            boxed(b"ftyp", b"isom\0\0\x02\0"),
            boxed(b"mdat", &[1, 2, 3]),
            moov(stco(&[40])),
            boxed(b"free", &[0; 16]),
        ]
        .concat();

        let mut stream = std::io::Cursor::new(original.clone());

        assert!(faststart_in_place(&mut stream, Path::new("stream")).is_err());
        assert_eq!(stream.into_inner(), original, "refused before anything was moved");
    }
}
