//! Video sources — input files with demuxing, decoding, and frame queuing.
//!
//! A source turns a file into pictures in display order. *When* each one is
//! shown is not its business: that is the timeline the compositor gives every
//! film (see [`crate::clock`]).

use std::collections::VecDeque;
use std::path::Path;
use std::time::Duration;

use crate::demux::Demuxer;
use crate::decode::Decoder;
use crate::error::{Error, Result};
use crate::frame::Frame;
use crate::plugin::Plugin;

/// How many decoded pictures a source keeps ready.
const QUEUE: usize = 30;

/// A video source: an input file with its demuxer, decoder, and frame queue.
///
/// VideoSource manages:
/// - Opening and parsing a media file
/// - Extracting the video track
/// - Decoding frames ahead into a bounded queue
/// - Seeking to an exact frame: the keyframe before it, then decoding forward
///   and discarding what comes first
/// - Serving ranges of frames from a [`FrameCache`] instead of the decoder
/// - Applying plugins to frames
pub struct VideoSource {
    /// Persistent, unique identifier for this source (used in layers and compositor).
    pub persistent_id: String,

    /// The demuxer (format + container handling).
    demuxer: Box<dyn Demuxer>,

    /// The decoder (codec handling).
    decoder: Box<dyn Decoder>,

    /// Track ID for the video stream.
    video_track_id: u32,

    /// How long one frame lasts, where the file says.
    frame_duration: Option<Duration>,

    /// Bounded frame queue, in display order.
    frame_queue: VecDeque<Frame>,

    /// Plugins to apply to each frame (in order).
    plugins: Vec<Box<dyn Plugin>>,

    /// Whether EOF has been reached.
    eof_reached: bool,

    /// After a seek: pictures due before this are decoded only to be thrown
    /// away, because decoding has to start at a keyframe.
    discard_before: Option<Duration>,

    /// The first picture's timestamp — frame zero. Not always zero: an MP4
    /// with B-frames starts its first picture a frame or two in.
    origin: Option<Duration>,

    /// Frames to serve from memory, with where the source is in them.
    cache: Option<Cached>,
}

/// A [`FrameCache`] in use.
struct Cached {
    cache: FrameCache,
    /// The next frame, by number, that the queue needs.
    next: u64,
    /// Whether the decoder is positioned to produce `next`. False after
    /// serving frames from memory: the decoder is still wherever it stopped.
    decoder_in_step: bool,
}

impl VideoSource {
    /// Opens a file: its container, its video track, and a decoder for it.
    ///
    /// # Errors
    ///
    /// Whatever opening the container refuses; [`Error::Unsupported`] for a
    /// file with no video; [`Error::NoDecoder`] where nothing here decodes it.
    #[cfg(feature = "demux")]
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();

        // Open demuxer
        let demuxer = crate::open_media(path)?;

        // Find video track
        let media_info = demuxer.info();
        let video_track = media_info
            .video()
            .ok_or_else(|| crate::Error::Unsupported {
                what: format!("{}: no video track in it", path.display()),
            })?;
        let video_track_id = video_track.id;
        let frame_duration = video_track
            .frame_rate
            .map(|rate| rate.frame_duration())
            .filter(|duration| !duration.is_zero());

        // No decoder is an error, not a source that quietly produces no
        // frames: a black window with no message is the failure this crate
        // exists to avoid, and `open` already says which feature or platform
        // would have handled the file.
        let config = crate::decode::DecoderConfig::from_track(video_track)?;
        let decoder = crate::decode::open(&config)?;

        let persistent_id = format!("{:x}", {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut hasher = DefaultHasher::new();
            path.to_string_lossy().hash(&mut hasher);
            hasher.finish()
        });

        Ok(VideoSource {
            persistent_id,
            demuxer,
            decoder,
            video_track_id,
            frame_duration,
            frame_queue: VecDeque::with_capacity(QUEUE),
            plugins: Vec::new(),
            eof_reached: false,
            discard_before: None,
            origin: None,
            cache: None,
        })
    }

    /// Add a plugin to this source (applied in order).
    pub fn add_plugin(&mut self, plugin: Box<dyn Plugin>) {
        self.plugins.push(plugin);
    }

    /// Get the persistent ID (for use in layers).
    pub fn persistent_id(&self) -> &str {
        &self.persistent_id
    }

    /// What the file contains: its tracks, duration, and container.
    pub fn info(&self) -> &crate::media::MediaInfo {
        self.demuxer.info()
    }

    /// The decoder turning this source's packets into frames — worth asking
    /// whether it [is hardware](Decoder::is_hardware).
    pub fn decoder(&self) -> &dyn Decoder {
        self.decoder.as_ref()
    }

    /// How long one frame lasts, where the file states a frame rate.
    pub fn frame_duration(&self) -> Option<Duration> {
        self.frame_duration
    }

    /// Serves `cache`'s ranges from memory from now on, decoding only outside
    /// them, and goes back to the start.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] for a file that states no frame rate — a cache
    /// is addressed by frame number, which needs one — and whatever rewinding
    /// refuses.
    pub fn set_cache(&mut self, cache: FrameCache) -> Result<()> {
        if self.frame_duration.is_none() {
            return Err(Error::unsupported(
                "this file states no frame rate, and a frame cache is addressed by frame number",
            ));
        }

        self.origin()?;

        self.cache = Some(Cached {
            cache,
            next: 0,
            decoder_in_step: false,
        });

        self.rewind()
    }

    /// Advance playback by decoding frames and filling the queue.
    ///
    /// Pulls packets from the demuxer, decodes them, applies plugins, and
    /// queues frames (up to 30) — or takes them from the cache, where one
    /// covers them. When EOF is reached, flushes remaining frames from the
    /// decoder.
    pub fn advance(&mut self, _delta: Duration) -> Result<()> {
        while self.frame_queue.len() < QUEUE && !self.eof_reached {
            if self.serve_from_cache()? {
                continue;
            }

            // Get next packet from demuxer
            match self.demuxer.next_packet()? {
                Some(packet) => {
                    // Only process packets from the video track
                    if packet.track_id == self.video_track_id {
                        if let Some(frame) = self.decoder.decode(&packet)? {
                            self.take(frame)?;
                        }
                    }
                }
                None => {
                    // EOF reached, flush remaining frames from decoder
                    for frame in self.decoder.flush()? {
                        self.take(frame)?;
                    }

                    // The end of the file is the end of the film only if the
                    // decoder got there in step. If the flush ran into a
                    // cached range, that range is served next and decoding
                    // picks up after it; and a cache can hold the last frames
                    // outright.
                    let in_step = self
                        .cache
                        .as_ref()
                        .is_none_or(|cached| cached.decoder_in_step);

                    self.eof_reached = in_step && !self.cache_has_next();
                }
            }
        }

        Ok(())
    }

    /// The first picture's timestamp: where frame zero is, and what frame
    /// numbers count from.
    ///
    /// Usually zero, but not always — an MP4 with B-frames shows its first
    /// picture a frame or two in. Found by decoding the start of the film the
    /// first time it is asked, and remembered; the source is left at the
    /// start.
    ///
    /// # Errors
    ///
    /// Whatever seeking or decoding refuses.
    pub fn origin(&mut self) -> Result<Duration> {
        if let Some(origin) = self.origin {
            return Ok(origin);
        }

        self.frame_queue.clear();
        self.reposition(Duration::ZERO)?;

        // The decoder hands pictures out in display order, so the first one
        // out is the earliest.
        let origin = loop {
            match self.demuxer.next_packet()? {
                Some(packet) if packet.track_id == self.video_track_id => {
                    if let Some(frame) = self.decoder.decode(&packet)? {
                        break frame.pts();
                    }
                }
                Some(_) => {}
                None => {
                    break self
                        .decoder
                        .flush()?
                        .first()
                        .map_or(Duration::ZERO, Frame::pts)
                }
            }
        };

        self.origin = Some(origin);
        self.reposition(Duration::ZERO)?;

        Ok(origin)
    }

    /// Moves to `position` — a timestamp in the file's own terms, as frames
    /// carry them: the demuxer to the keyframe at or before it, the decoder
    /// and queue emptied, and every picture due before `position` decoded
    /// only to be thrown away — so the next frame out is the one due there,
    /// not the keyframe a second or two earlier. Frame `n` is at
    /// [`origin`](VideoSource::origin) plus `n` frame durations.
    ///
    /// # Errors
    ///
    /// Whatever the demuxer's seek or the decoder's reset refuses.
    pub fn seek(&mut self, position: Duration) -> Result<()> {
        self.frame_queue.clear();
        self.reposition(position)?;

        if let (Some(cached), Some(duration)) = (self.cache.as_mut(), self.frame_duration) {
            let origin = self.origin.unwrap_or_default();
            cached.next = frame_number(position.saturating_sub(origin), duration);
            cached.decoder_in_step = true;
        }

        Ok(())
    }

    /// The decoder to `position`, leaving the queue alone: what moving from a
    /// cached range back to decoding needs, with the cached frames still
    /// waiting to be shown.
    fn reposition(&mut self, position: Duration) -> Result<()> {
        self.demuxer.seek(position)?;
        self.decoder.reset()?;
        self.eof_reached = false;
        // Half a frame of slack: a picture due a hair before the target, by
        // rounding, is the target.
        let slack = self.frame_duration.unwrap_or_default() / 2;
        self.discard_before = Some(position.saturating_sub(slack)).filter(|at| !at.is_zero());

        Ok(())
    }

    /// Back to the start: the demuxer to the first keyframe, the decoder and
    /// the queue emptied. What looping a film needs, without reopening it.
    pub fn rewind(&mut self) -> Result<()> {
        self.seek(Duration::ZERO)
    }

    /// Get the next frame if available (without removing it from the queue).
    pub fn current_frame(&self) -> Option<&Frame> {
        self.frame_queue.front()
    }

    /// Pop the next frame from the queue (for rendering).
    pub fn pop_frame(&mut self) -> Option<Frame> {
        self.frame_queue.pop_front()
    }

    /// Number of frames in the queue.
    pub fn queued_frames(&self) -> usize {
        self.frame_queue.len()
    }

    /// Whether EOF has been reached and all frames consumed.
    pub fn is_finished(&self) -> bool {
        self.eof_reached && self.frame_queue.is_empty()
    }

    /// One decoded picture: dropped if a seek or the cache says so, otherwise
    /// through the plugins and into the queue.
    fn take(&mut self, frame: Frame) -> Result<()> {
        if let Some(before) = self.discard_before {
            if frame.pts() < before {
                return Ok(());
            }

            self.discard_before = None;
        }

        if let (Some(cached), Some(duration)) = (self.cache.as_mut(), self.frame_duration) {
            let origin = self.origin.unwrap_or_default();
            let number = frame_number(frame.pts().saturating_sub(origin), duration);

            // Already served, from memory or before a seek.
            if number < cached.next {
                return Ok(());
            }

            // Decoding has run into a cached range: memory takes over, and
            // the decoder will have to be put back in step after it.
            if cached.cache.get(number).is_some() {
                cached.decoder_in_step = false;
                return Ok(());
            }

            cached.next = number + 1;
        }

        let mut frame = frame;
        for plugin in &mut self.plugins {
            frame = plugin.process(frame)?;
        }

        self.frame_queue.push_back(frame);
        Ok(())
    }

    /// Queues the next frame from memory if the cache has it. Where it does
    /// not, puts the decoder in step for that frame if it has wandered, and
    /// returns false so the caller decodes.
    fn serve_from_cache(&mut self) -> Result<bool> {
        let (Some(cached), Some(duration)) = (self.cache.as_mut(), self.frame_duration) else {
            return Ok(false);
        };
        let origin = self.origin.unwrap_or_default();

        if let Some(frame) = cached.cache.get(cached.next) {
            // Retimed to where it belongs in the stream, whatever it was
            // stamped with when it was made.
            let frame = frame.with_pts(origin + frame_time(cached.next, duration));
            cached.next += 1;
            cached.decoder_in_step = false;

            let mut frame = frame;
            for plugin in &mut self.plugins {
                frame = plugin.process(frame)?;
            }

            self.frame_queue.push_back(frame);
            return Ok(true);
        }

        if !cached.decoder_in_step {
            cached.decoder_in_step = true;
            let position = origin + frame_time(cached.next, duration);

            self.reposition(position)?;
        }

        Ok(false)
    }

    /// Whether the cache holds the next frame the queue needs.
    fn cache_has_next(&self) -> bool {
        self.cache
            .as_ref()
            .is_some_and(|cached| cached.cache.get(cached.next).is_some())
    }
}

/// The frame due at `position`, counting from zero at the frame rate.
fn frame_number(position: Duration, frame_duration: Duration) -> u64 {
    let frame = frame_duration.as_nanos().max(1);
    // Rounded, not floored: a timestamp a nanosecond short of a frame
    // boundary, from a timescale that does not divide evenly, is that frame.
    ((position.as_nanos() + frame / 2) / frame) as u64
}

/// When frame `number` is due, counting from zero at the frame rate.
fn frame_time(number: u64, frame_duration: Duration) -> Duration {
    let nanos = u128::from(number) * frame_duration.as_nanos();
    Duration::from_nanos(nanos.min(u128::from(u64::MAX)) as u64)
}

/// Decoded frames held in memory for ranges of a film, so those ranges play
/// without the decoder — an instant start, a loop that never waits on a
/// keyframe.
///
/// Ranges are frame numbers, `start` included and `end` not, counted from the
/// first frame at the file's frame rate. The frames are the ranges' frames,
/// in order, end to end.
#[derive(Clone, Debug)]
pub struct FrameCache {
    ranges: Vec<(u64, u64)>,
    /// Where each range's first frame sits in `frames`.
    offsets: Vec<usize>,
    frames: Vec<Frame>,
}

impl FrameCache {
    /// A cache of `frames` for `ranges`.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] for an empty or backwards range, ranges that
    /// overlap, or a number of frames that is not what the ranges add up to —
    /// each of which would serve the wrong picture rather than fail later.
    pub fn new(mut ranges: Vec<(u64, u64)>, frames: Vec<Frame>) -> Result<Self> {
        ranges.sort_unstable();

        let mut offsets = Vec::with_capacity(ranges.len());
        let mut total = 0_usize;
        let mut previous_end = 0_u64;

        for (index, &(start, end)) in ranges.iter().enumerate() {
            if start >= end {
                return Err(Error::unsupported(format!(
                    "frame range {start}..{end} holds no frames"
                )));
            }

            if index > 0 && start < previous_end {
                return Err(Error::unsupported(format!(
                    "frame range {start}..{end} overlaps the one before it, which ends at {previous_end}"
                )));
            }

            offsets.push(total);
            total += usize::try_from(end - start).map_err(|_| {
                Error::unsupported(format!("frame range {start}..{end} is too long to hold"))
            })?;
            previous_end = end;
        }

        if total != frames.len() {
            return Err(Error::unsupported(format!(
                "the ranges cover {total} frames and {} were given",
                frames.len()
            )));
        }

        Ok(FrameCache {
            ranges,
            offsets,
            frames,
        })
    }

    /// The frame numbered `number`, if a range covers it.
    pub fn get(&self, number: u64) -> Option<&Frame> {
        let index = self
            .ranges
            .partition_point(|&(start, _)| start <= number)
            .checked_sub(1)?;
        let (start, end) = self.ranges[index];

        if number >= end {
            return None;
        }

        self.frames
            .get(self.offsets[index] + (number - start) as usize)
    }

    /// The ranges, sorted.
    pub fn ranges(&self) -> &[(u64, u64)] {
        &self.ranges
    }

    /// How many frames it holds.
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// Whether it holds none.
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// The memory its pictures take, for holding a show to a budget.
    pub fn byte_len(&self) -> usize {
        self.frames.iter().map(Frame::byte_len).sum()
    }
}

/// Decodes `ranges` of the film at `path` into the frames a [`FrameCache`]
/// for those ranges wants — what [`Controls::add_cached`](crate::Controls)
/// takes.
///
/// Each range costs a seek to the keyframe before it and decoding forward;
/// files `import` writes have a keyframe every two seconds, so that is short.
///
/// # Errors
///
/// Whatever opening or decoding the file refuses, [`Error::Unsupported`] for a
/// file with no frame rate, and [`Error::Decode`] for a range that runs past
/// the end of the film.
#[cfg(feature = "demux")]
pub fn cache_frames(path: impl AsRef<Path>, ranges: &[(u64, u64)]) -> Result<Vec<Frame>> {
    let mut source = VideoSource::from_file(path)?;
    let duration = source.frame_duration().ok_or_else(|| {
        Error::unsupported("this file states no frame rate, so its frames cannot be numbered")
    })?;

    let origin = source.origin()?;

    let mut sorted = ranges.to_vec();
    sorted.sort_unstable();

    let mut frames = Vec::new();

    for (start, end) in sorted {
        source.seek(origin + frame_time(start, duration))?;
        let mut number = start;

        while number < end {
            source.advance(Duration::ZERO)?;

            let Some(frame) = source.pop_frame() else {
                return Err(Error::Decode {
                    encoding: source.decoder().encoding(),
                    reason: format!(
                        "frame range {start}..{end} runs past the end of the film, at frame {number}"
                    ),
                });
            };

            frames.push(frame);
            number += 1;
        }
    }

    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::ColorSpace;
    use crate::frame::PixelFormat;

    fn frame(tag: u8) -> Frame {
        Frame::packed(
            2,
            2,
            PixelFormat::Rgba8,
            ColorSpace::srgb(),
            Duration::ZERO,
            vec![tag; 16],
        )
        .unwrap()
    }

    fn tag(frame: &Frame) -> u8 {
        frame.data()[0]
    }

    #[test]
    fn a_cache_finds_each_frame_by_number() {
        let cache = FrameCache::new(
            vec![(10, 12), (0, 2)],
            vec![frame(0), frame(1), frame(10), frame(11)],
        )
        .unwrap();

        assert_eq!(cache.get(0).map(tag), Some(0));
        assert_eq!(cache.get(1).map(tag), Some(1));
        assert!(cache.get(2).is_none());
        assert!(cache.get(9).is_none());
        assert_eq!(cache.get(10).map(tag), Some(10));
        assert_eq!(cache.get(11).map(tag), Some(11));
        assert!(cache.get(12).is_none());
        assert_eq!(cache.len(), 4);
    }

    /// Each of these would serve the wrong picture later rather than fail now.
    #[test]
    fn a_cache_that_does_not_add_up_is_refused() {
        assert!(FrameCache::new(vec![(0, 3)], vec![frame(0), frame(1)]).is_err());
        assert!(FrameCache::new(vec![(5, 5)], vec![]).is_err());
        assert!(FrameCache::new(vec![(0, 4), (2, 6)], vec![frame(0); 8]).is_err());
    }

    #[test]
    fn frame_numbers_round_rather_than_floor() {
        let frame = Duration::from_nanos(41_708_333);

        assert_eq!(frame_number(Duration::ZERO, frame), 0);
        assert_eq!(frame_number(frame * 24 - Duration::from_nanos(3), frame), 24);
        assert_eq!(frame_number(frame * 24 + Duration::from_nanos(3), frame), 24);
    }
}
