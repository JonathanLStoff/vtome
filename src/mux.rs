//! Putting packets back into a container: H.264 into MP4, AV1 into WebM.
//!
//! The container follows the codec, never the output path's extension —
//! the same rule identification follows on the way in. H.264 goes into MP4
//! because that is what every hardware decoder and every WebView expects it
//! in; AV1 into WebM, through a small Matroska writer of vtome's own, because
//! the `mp4` crate writes no `av01` sample entry.
//!
//! Both write what `planning/TODO.md` §15 asks of a file, as far as the
//! container is concerned:
//!
//! - MP4: `moov` before `mdat` ("faststart"). The `mp4` crate can only write
//!   the index last, once it knows it, so the file is rewritten with the index
//!   moved to the front when the stream ends.
//! - WebM: a `SeekHead`, a `Duration`, a cluster per keyframe, and `Cues`
//!   pointing at every one, so a seek is one lookup rather than a scan.
//!
//! Neither holds a file in memory: samples go to disk as they come, and the
//! faststart rewrite copies the media through in blocks.
//!
//! The destination is a path ([`create`]) or any [`Storage`] ([`create_in`]) —
//! a file already open, or an entry of a bundle. Both containers go back and
//! patch what they wrote, and MP4 reads its own media back to move the index,
//! so the destination has to read and seek as well as write; it also has to
//! start empty, at its start.

use std::fs::File;
use std::io::{Read, Seek, Write};
use std::path::Path;
use std::time::Duration;

use crate::color::ColorSpace;
use crate::error::{Error, Result};
use crate::identify::{Container, Encoding};
use crate::media::{Packet, Rational};

mod mp4;
mod webm;

/// Somewhere a muxer can write a whole file: [`Read`] + [`Write`] + [`Seek`],
/// and [`Send`] because the muxer is.
///
/// Implemented for everything that is all four, which is what a [`File`] is and
/// what a `pfac` bundle entry is. `&mut W` is too, so a writer can be lent and
/// kept: its owner usually has something left to do with it.
pub trait Storage: Read + Write + Seek + Send {}

impl<T: Read + Write + Seek + Send + ?Sized> Storage for T {}

/// The video track being written.
#[derive(Clone, Debug)]
pub struct VideoTrack {
    /// H.264 or AV1.
    pub encoding: Encoding,
    /// Picture width.
    pub width: u32,
    /// Picture height.
    pub height: u32,
    /// Pictures a second: each sample's duration, and the last one's.
    pub frame_rate: Rational,
    /// Written into the container where it has a place for it (WebM's
    /// `Colour`); MP4 relies on the SPS, which the encoder tagged.
    pub color: ColorSpace,
    /// The encoder's `avcC` or `av1C`.
    pub config_record: Vec<u8>,
}

/// What a finished file holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Written {
    /// Samples written.
    pub samples: u64,
    /// From the first sample's start to the last one's end.
    pub duration: Duration,
    /// The file's size.
    pub bytes: u64,
}

/// Something packets go into, one video track at a time.
pub trait Muxer: Send {
    /// Adds one packet. Packets arrive in decode order with rising decode
    /// times; each one's duration is the gap to the next.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if the file cannot be written; [`Error::Encode`] for a
    /// packet whose time runs backwards.
    fn write(&mut self, packet: Packet) -> Result<()>;

    /// Writes the index and closes the file.
    ///
    /// # Errors
    ///
    /// As [`write`](Muxer::write).
    fn finish(self: Box<Self>) -> Result<Written>;
}

/// The container `encoding` is written in.
///
/// # Errors
///
/// [`Error::NoEncoder`] for an encoding vtome does not write.
pub fn container_for(encoding: Encoding) -> Result<Container> {
    match encoding {
        Encoding::H264 => Ok(Container::Mp4),
        Encoding::Av1 => Ok(Container::WebM),
        other => Err(Error::NoEncoder {
            encoding: other,
            remedy: "vtome writes H.264 and AV1 only".to_string(),
        }),
    }
}

/// Starts a file at `path` for `track`, in the container its encoding goes
/// in. Whatever was at `path` is replaced.
///
/// # Errors
///
/// [`Error::Io`] if the file cannot be created; [`Error::Encode`] for a
/// configuration record the container cannot use.
pub fn create(path: &Path, track: VideoTrack) -> Result<Box<dyn Muxer>> {
    let open = || File::create(path).map_err(|error| Error::io(path, error));

    match container_for(track.encoding)? {
        Container::Mp4 => Ok(Box::new(mp4::Mp4Muxer::create(open, path, false, track)?)),
        _ => Ok(Box::new(webm::WebmMuxer::create(open, path, |file| file.sync_all(), track)?)),
    }
}

/// [`create`], into `storage` rather than a path.
///
/// `storage` must be positioned at its start and empty from there: both
/// containers record absolute positions, so a file that begins partway into a
/// stream would be pointing at the wrong bytes. When it is done `storage`
/// holds a complete file, flushed, positioned at its end; closing it, syncing
/// it, and deciding what to do with a half-written one after an error are its
/// owner's.
///
/// # Errors
///
/// As [`create`], and [`Error::Encode`] for a `storage` that is not at its
/// start.
pub fn create_in<'a>(storage: &'a mut dyn Storage, track: VideoTrack) -> Result<Box<dyn Muxer + 'a>> {
    let label = Path::new(STORAGE_LABEL);
    let open = move || Ok(storage);

    match container_for(track.encoding)? {
        Container::Mp4 => Ok(Box::new(mp4::Mp4Muxer::create(open, label, true, track)?)),
        _ => Ok(Box::new(webm::WebmMuxer::create(open, label, |_| Ok(()), track)?)),
    }
}

/// What errors call a destination that has no path.
const STORAGE_LABEL: &str = "the output";

/// Checks that `storage` is at its start, which both containers rely on.
fn require_start(storage: &mut impl Seek, label: &Path) -> Result<()> {
    let position = storage.stream_position().map_err(|error| Error::io(label, error))?;

    if position == 0 {
        Ok(())
    } else {
        Err(mux_error(format!(
            "{}: a file is written from its start, and this is {position} bytes in",
            label.display()
        )))
    }
}

/// How long one frame lasts at `rate`, never zero.
fn frame_duration(rate: Rational) -> Duration {
    let duration = rate.frame_duration();

    if duration.is_zero() {
        Duration::from_millis(40)
    } else {
        duration
    }
}

fn mux_error(reason: impl Into<String>) -> Error {
    Error::Encode {
        reason: reason.into(),
    }
}
