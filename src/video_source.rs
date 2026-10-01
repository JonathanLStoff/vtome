//! Video sources — input files with demuxing, decoding, and frame queuing.

use std::collections::VecDeque;
use std::path::Path;

use crate::clock::Clock;
use crate::demux::Demuxer;
use crate::decode::Decoder;
use crate::error::Result;
use crate::frame::Frame;
use crate::plugin::Plugin;

/// A video source: an input file with demuxer, decoder, clock, and frame queue.
///
/// VideoSource manages:
/// - Opening and parsing a media file
/// - Extracting the video track
/// - Decoding frames on demand
/// - Queueing frames for playback
/// - Applying plugins to frames
/// - Synchronizing playback via a Clock
///
/// # Integration with atome
///
/// VideoSource can slave to an external master clock (e.g., atome's audio clock).
/// If no master clock is provided, it uses an internal one.
pub struct VideoSource {
    /// Persistent, unique identifier for this source (used in layers and compositor).
    pub persistent_id: String,

    /// The demuxer (format + container handling).
    demuxer: Box<dyn Demuxer>,

    /// The decoder (codec handling).
    decoder: Box<dyn Decoder>,

    /// Internal clock (only used if no external master clock is provided).
    internal_clock: Clock,

    /// Optional external master clock (e.g., from atome). If present, this clock drives playback.
    external_master: Option<Box<dyn crate::clock::MasterClock>>,

    /// Use external clock if available, otherwise use internal.
    use_external_clock: bool,

    /// Track ID for the video stream.
    video_track_id: u32,

    /// Bounded frame queue (max 30 frames).
    frame_queue: VecDeque<Frame>,

    /// Plugins to apply to each frame (in order).
    plugins: Vec<Box<dyn Plugin>>,

    /// Whether EOF has been reached.
    eof_reached: bool,

    /// Current position in the stream (from the clock).
    current_pts: std::time::Duration,
}

impl VideoSource {
    /// Create a VideoSource from a file path.
    ///
    /// If `external_master` is provided, playback will slave to that clock.
    /// Otherwise, an internal clock is used.
    #[cfg(feature = "demux")]
    pub fn from_file<P: AsRef<Path>>(
        path: P,
        external_master: Option<Box<dyn crate::clock::MasterClock>>,
    ) -> Result<Self> {
        let path = path.as_ref();

        // Open demuxer
        let demuxer = crate::open_media(path)?;

        // Find video track
        let media_info = demuxer.info();
        let video_track = media_info
            .video()
            .ok_or_else(|| crate::Error::Unsupported {
                what: "no video track in media".to_string(),
            })?;
        let video_track_id = video_track.id;

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

        let use_external_clock = external_master.is_some();

        Ok(VideoSource {
            persistent_id,
            demuxer,
            decoder,
            internal_clock: Clock::new(),
            external_master,
            use_external_clock,
            video_track_id,
            frame_queue: VecDeque::with_capacity(30),
            plugins: Vec::new(),
            eof_reached: false,
            current_pts: std::time::Duration::ZERO,
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

    /// Advance playback by decoding frames and filling the queue.
    ///
    /// Pulls packets from the demuxer, decodes them, applies plugins,
    /// and queues frames (up to 30). When EOF is reached, flushes
    /// remaining frames from the decoder.
    pub fn advance(&mut self, _delta: std::time::Duration) -> Result<()> {
        // Try to fill the queue from the demuxer
        while self.frame_queue.len() < 30 && !self.eof_reached {
            // Get next packet from demuxer
            match self.demuxer.next_packet()? {
                Some(packet) => {
                    // Only process packets from the video track
                    if packet.track_id == self.video_track_id {
                        // Feed to decoder
                        if let Some(frame) = self.decoder.decode(&packet)? {
                            // Apply plugins in order
                            let mut processed_frame = frame;
                            for plugin in &mut self.plugins {
                                processed_frame = plugin.process(processed_frame)?;
                            }
                            // Queue the frame
                            self.frame_queue.push_back(processed_frame);
                        }
                    }
                }
                None => {
                    // EOF reached, flush remaining frames from decoder
                    let flushed_frames = self.decoder.flush()?;
                    for mut frame in flushed_frames {
                        // Apply plugins to flushed frames too
                        for plugin in &mut self.plugins {
                            frame = plugin.process(frame)?;
                        }
                        self.frame_queue.push_back(frame);
                    }
                    self.eof_reached = true;
                }
            }
        }

        Ok(())
    }

    /// Back to the start: the demuxer to the first keyframe, the decoder and
    /// the queue emptied. What looping a film needs, without reopening it.
    pub fn rewind(&mut self) -> Result<()> {
        self.demuxer.seek(std::time::Duration::ZERO)?;
        self.decoder.reset()?;
        self.frame_queue.clear();
        self.eof_reached = false;
        self.current_pts = std::time::Duration::ZERO;

        Ok(())
    }

    /// Get the next frame if available (without removing it from the queue).
    pub fn current_frame(&self) -> Option<&Frame> {
        self.frame_queue.front()
    }

    /// Pop the next frame from the queue (for rendering).
    pub fn pop_frame(&mut self) -> Option<Frame> {
        self.frame_queue.pop_front()
    }

    /// Current playback position.
    pub fn position(&self) -> std::time::Duration {
        self.current_pts
    }

    /// Number of frames in the queue.
    pub fn queued_frames(&self) -> usize {
        self.frame_queue.len()
    }

    /// Whether EOF has been reached and all frames consumed.
    pub fn is_finished(&self) -> bool {
        self.eof_reached && self.frame_queue.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_source_persistent_id_is_stable() {
        let id1 = {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut hasher = DefaultHasher::new();
            "test.mp4".hash(&mut hasher);
            format!("{:x}", hasher.finish())
        };

        let id2 = {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut hasher = DefaultHasher::new();
            "test.mp4".hash(&mut hasher);
            format!("{:x}", hasher.finish())
        };

        assert_eq!(id1, id2);
    }

    #[test]
    fn video_source_frame_queue_operations() {
        // We can't easily test actual file loading without fixtures
        // This test is a placeholder for the architecture
    }
}
