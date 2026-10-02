//! A film's timing: which of its pictures belongs on screen now.
//!
//! Shared by everything that plays video — [`Viewer`](crate::window::Viewer)
//! for one film in one place, the [`Compositor`](crate::Compositor) for many
//! in layers. Each picture goes up when its own timestamp says, against a
//! clock that starts once the first one is actually on screen; late pictures
//! are dropped rather than shown late, so a stall costs a few frames instead
//! of permanent lag.

use std::time::Duration;

use crate::clock::{Action, Follower, MasterClock, Pacing, SharedClock};
use crate::error::{Error, Result};
use crate::frame::Frame;
use crate::video_source::VideoSource;

/// A film while it plays.
pub(crate) struct Playback {
    source: VideoSource,
    /// The film's position, on whichever timeline it was given — the engine's,
    /// which is atome's when there is audio.
    clock: Follower,
    pacing: Pacing,
    /// Start again at the end, or finish.
    pub(crate) looping: bool,
    /// Whether the clock has started. Unless the film was given a start on the
    /// timeline, it waits for the first picture to be on screen, so opening
    /// the GPU, filling the queue, and a window appearing are not counted
    /// against the film's first second.
    started: bool,
    draws_before_start: u8,
    /// The first picture's timestamp. An MP4 with B-frames starts a frame or
    /// two in, and the clock starts at zero.
    origin: Option<Duration>,
    /// When the picture on screen is due, relative to `origin`.
    showing: Option<Duration>,
    /// How long the last picture stays up before the film ends or loops.
    frame_duration: Duration,
    loops: u32,
}

/// What the next refresh should do.
pub(crate) enum Tick {
    /// This picture is due: upload it and draw it.
    Show(Frame),
    /// Keep showing what is already up.
    Hold,
    /// The film is over and does not loop.
    Ended,
}

impl Playback {
    /// A film on `timeline`, starting once its first picture is on screen —
    /// or, with `start_at`, exactly when the timeline reaches that point, and
    /// showing nothing until then. That is how a picture lines up with a sound
    /// scheduled in atome for the same moment.
    pub(crate) fn new(
        source: VideoSource,
        looping: bool,
        timeline: SharedClock,
        start_at: Option<Duration>,
    ) -> Self {
        let rate = source.info().video().and_then(|track| track.frame_rate);

        let clock = match start_at {
            Some(at) => Follower::starting_at(timeline, at),
            None => Follower::new(timeline),
        };

        Playback {
            source,
            clock,
            pacing: Pacing::for_frame_rate(rate.map_or(25.0, |rate| rate.as_f64())),
            looping,
            started: start_at.is_some(),
            draws_before_start: 0,
            origin: None,
            showing: None,
            frame_duration: rate.map_or(Duration::from_millis(40), |rate| rate.frame_duration()),
            loops: 0,
        }
    }

    pub(crate) fn source(&self) -> &VideoSource {
        &self.source
    }

    pub(crate) fn source_mut(&mut self) -> &mut VideoSource {
        &mut self.source
    }

    /// The picture's size as stored, known before anything decodes. Not as
    /// rotated: the renderer draws the planes as they are.
    // Only `Viewer` reports on a single film.
    #[cfg_attr(not(feature = "window"), allow(dead_code))]
    pub(crate) fn size(&self) -> (u32, u32) {
        self.source
            .info()
            .video()
            .map_or((1, 1), |track| (track.width, track.height))
    }

    /// Presented, dropped, and repeated, as [`Pacing`] counts them.
    // Only `Viewer` reports on a single film.
    #[cfg_attr(not(feature = "window"), allow(dead_code))]
    pub(crate) fn counts(&self) -> (u64, u64, u64) {
        self.pacing.counts()
    }

    /// How many times the film has started again from the top.
    // Only `Viewer` reports on a single film.
    #[cfg_attr(not(feature = "window"), allow(dead_code))]
    pub(crate) fn loops(&self) -> u32 {
        self.loops
    }

    /// Where the film is, on its own clock.
    pub(crate) fn position(&self) -> Duration {
        self.clock.position()
    }

    /// Decodes ahead and decides what this refresh shows.
    pub(crate) fn tick(&mut self) -> Result<Tick> {
        self.source.advance(Duration::ZERO)?;

        // Given a start on the timeline that is still ahead: decode ready,
        // show nothing.
        if !self.clock.is_due() {
            return Ok(Tick::Hold);
        }

        let origin = *self
            .origin
            .get_or_insert_with(|| self.source.current_frame().map_or(Duration::ZERO, Frame::pts));

        let now = self.clock.position();

        if self.source.is_finished() {
            let Some(last) = self.showing else {
                return Err(Error::Decode {
                    encoding: self.source.decoder().encoding(),
                    reason: "the file decoded to no pictures at all".to_string(),
                });
            };

            // The last picture gets its full time on screen before the end.
            if now < last + self.frame_duration {
                return Ok(Tick::Hold);
            }

            if !self.looping {
                return Ok(Tick::Ended);
            }

            self.source.rewind()?;
            self.origin = None;
            self.showing = None;
            self.clock.seek(Duration::ZERO);
            self.loops += 1;

            // Once: after a rewind the queue refills, and a file with nothing
            // in it has already been refused above.
            return self.tick();
        }

        // Pacing decides for the picture at the head of the queue: show it,
        // throw it away as too late, or leave the current one up a while.
        while let Some(next) = self.source.current_frame() {
            match self.pacing.decide(next.pts().saturating_sub(origin), now) {
                Action::Present => {
                    if let Some(frame) = self.source.pop_frame() {
                        self.showing = Some(frame.pts().saturating_sub(origin));
                        return Ok(Tick::Show(frame));
                    }
                }
                Action::Drop => {
                    self.source.pop_frame();
                }
                Action::Wait(_) => break,
            }
        }

        if self.showing.is_some() {
            self.pacing.note_repeat();
        }

        Ok(Tick::Hold)
    }

    /// Called after every presentation. The second one starts the clock.
    ///
    /// Not the first: drawing into a brand-new window returns at once, and it
    /// is the *next* refresh that waits while the window server puts the
    /// window on screen — about 200 ms on macOS. A clock already running
    /// through that wait drops the film's first handful of frames.
    pub(crate) fn presented(&mut self) {
        if self.started || self.showing.is_none() {
            return;
        }

        self.draws_before_start += 1;

        if self.draws_before_start >= 2 {
            self.started = true;
            self.clock.play();
        }
    }

    // Only `Viewer` reports on a single film.
    #[cfg_attr(not(feature = "window"), allow(dead_code))]
    pub(crate) fn toggle_pause(&mut self) {
        if !self.started {
            return;
        }

        if self.clock.is_playing() {
            self.clock.pause();
        } else {
            self.clock.play();
        }
    }
}
