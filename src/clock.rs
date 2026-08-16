//! When to show a frame, and what to do when it is late.
//!
//! Playback is a question of timing rather than of decoding: the decoder
//! produces pictures with presentation stamps, and something has to decide
//! which one belongs on screen right now. That decision is here, separated from
//! anything that draws, so it can be tested without a GPU and against a clock
//! that does not have to be real time.
//!
//! # Audio is the master, always
//!
//! When vtome runs next to `atome`, the audio clock leads and video follows. A
//! dropped frame is invisible at 24 fps; a resampled or stuttered audio buffer
//! is immediately audible. So [`Clock`] can be slaved to any [`MasterClock`] —
//! implement it over the audio engine's play position and video will chase it.

use std::time::{Duration, Instant};

/// Something that knows what time it is in a stream.
///
/// The audio engine implements this over its own play position, which is the
/// only clock in the system that is not allowed to drift.
pub trait MasterClock: Send + Sync {
    /// How far into the stream playback currently is.
    fn position(&self) -> Duration;

    /// Whether it is running. A paused master freezes video too.
    fn is_running(&self) -> bool {
        true
    }
}

/// A clock of vtome's own, driven by the machine's monotonic time.
///
/// The fallback when there is no audio to follow.
#[derive(Debug)]
pub struct Clock {
    /// Where playback was when it last started or was seeked.
    anchor_position: Duration,
    /// When that was, in monotonic time. `None` while paused.
    anchor_instant: Option<Instant>,
    rate: f64,
}

impl Default for Clock {
    fn default() -> Self {
        Clock::new()
    }
}

impl Clock {
    /// A clock at zero, paused.
    pub fn new() -> Self {
        Clock {
            anchor_position: Duration::ZERO,
            anchor_instant: None,
            rate: 1.0,
        }
    }

    /// Starts, or resumes, from wherever it is.
    pub fn play(&mut self) {
        if self.anchor_instant.is_none() {
            self.anchor_instant = Some(Instant::now());
        }
    }

    /// Stops, keeping the position.
    pub fn pause(&mut self) {
        if self.anchor_instant.is_some() {
            // Fold elapsed time into the anchor before dropping it, or the
            // pause would rewind to wherever the last seek was.
            self.anchor_position = self.position();
            self.anchor_instant = None;
        }
    }

    /// Whether it is running.
    pub fn is_running(&self) -> bool {
        self.anchor_instant.is_some()
    }

    /// Moves to a position. Keeps running if it was running.
    pub fn seek(&mut self, position: Duration) {
        self.anchor_position = position;

        if self.anchor_instant.is_some() {
            self.anchor_instant = Some(Instant::now());
        }
    }

    /// Playback speed. 1.0 is normal, 0.5 is half.
    ///
    /// Clamped to something sane: a rate of zero is what `pause` is for, and a
    /// negative one is not implemented — decoders run forwards.
    pub fn set_rate(&mut self, rate: f64) {
        // Fold in the time spent at the old rate before changing it.
        if self.anchor_instant.is_some() {
            self.anchor_position = self.position();
            self.anchor_instant = Some(Instant::now());
        }

        self.rate = rate.clamp(0.01, 16.0);
    }

    /// The current rate.
    pub fn rate(&self) -> f64 {
        self.rate
    }
}

impl MasterClock for Clock {
    fn position(&self) -> Duration {
        match self.anchor_instant {
            Some(started) => self.anchor_position + started.elapsed().mul_f64(self.rate),
            None => self.anchor_position,
        }
    }

    fn is_running(&self) -> bool {
        self.anchor_instant.is_some()
    }
}

/// What to do with a frame that is due at a given time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Draw it now.
    Present,
    /// Too early: wait this long and ask again. Not a sleep — a caller with a
    /// vsync to wait on has better things to block on.
    Wait(Duration),
    /// Too late to be worth drawing: throw it away and take the next one.
    ///
    /// The alternative is drawing every frame however late it is, which turns a
    /// brief stall into permanent lag.
    Drop,
}

/// The decision, and the count of how often it has gone each way.
///
/// Kept as a struct rather than a free function because the counters are what
/// makes a stuttering player diagnosable: dropped frames mean the decoder is
/// behind, repeats mean the display is faster than the content.
#[derive(Clone, Copy, Debug, Default)]
pub struct Pacing {
    /// How late a frame may be and still be worth drawing.
    tolerance: Duration,
    presented: u64,
    dropped: u64,
    repeated: u64,
}

impl Pacing {
    /// Half a frame interval of tolerance, which is the usual choice: a frame
    /// more than half an interval late would be replaced by the next one before
    /// anyone saw it.
    pub fn for_frame_rate(frames_per_second: f64) -> Self {
        let interval = if frames_per_second > 0.0 {
            Duration::from_secs_f64(1.0 / frames_per_second)
        } else {
            Duration::from_millis(40)
        };

        Pacing {
            tolerance: interval / 2,
            ..Pacing::default()
        }
    }

    /// A pacer with an explicit tolerance.
    pub fn with_tolerance(tolerance: Duration) -> Self {
        Pacing {
            tolerance,
            ..Pacing::default()
        }
    }

    /// What to do with a frame due at `frame_pts` when the master says `now`.
    pub fn decide(&mut self, frame_pts: Duration, now: Duration) -> Action {
        if frame_pts > now {
            let wait = frame_pts - now;

            // Within tolerance of due counts as due: waiting a millisecond to
            // be exactly on time misses the vsync it was aiming for.
            if wait <= self.tolerance {
                self.presented += 1;
                return Action::Present;
            }

            return Action::Wait(wait - self.tolerance);
        }

        let late = now - frame_pts;

        if late > self.tolerance {
            self.dropped += 1;
            return Action::Drop;
        }

        self.presented += 1;
        Action::Present
    }

    /// Records that the previous frame was shown again because no new one was
    /// ready.
    pub fn note_repeat(&mut self) {
        self.repeated += 1;
    }

    /// Presented, dropped, repeated.
    pub fn counts(&self) -> (u64, u64, u64) {
        (self.presented, self.dropped, self.repeated)
    }

    /// The share of frames that were dropped, for a health check that does not
    /// require the caller to do the arithmetic.
    pub fn drop_rate(&self) -> f64 {
        let total = self.presented + self.dropped;

        if total == 0 {
            return 0.0;
        }

        self.dropped as f64 / total as f64
    }

    /// Forgets the counters, without changing the tolerance.
    pub fn reset(&mut self) {
        self.presented = 0;
        self.dropped = 0;
        self.repeated = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clock that says whatever the test wants, so pacing can be checked
    /// without waiting in real time.
    #[derive(Debug, Default)]
    struct FixedClock(Duration);

    impl MasterClock for FixedClock {
        fn position(&self) -> Duration {
            self.0
        }
    }

    #[test]
    fn a_new_clock_is_paused_at_zero() {
        let clock = Clock::new();

        assert_eq!(clock.position(), Duration::ZERO);
        assert!(!clock.is_running());
    }

    #[test]
    fn pausing_keeps_the_position_and_stops_it_advancing() {
        let mut clock = Clock::new();
        clock.seek(Duration::from_secs(5));
        clock.play();

        std::thread::sleep(Duration::from_millis(20));
        clock.pause();

        let paused_at = clock.position();
        assert!(paused_at >= Duration::from_secs(5));

        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(clock.position(), paused_at, "a paused clock moved");
    }

    /// The bug this shape exists to prevent: pausing that rewinds to the last
    /// seek because elapsed time was never folded in.
    #[test]
    fn pausing_twice_does_not_rewind() {
        let mut clock = Clock::new();
        clock.play();
        std::thread::sleep(Duration::from_millis(20));

        clock.pause();
        let first = clock.position();
        clock.pause();

        assert_eq!(clock.position(), first);
    }

    #[test]
    fn rate_changes_do_not_lose_the_position() {
        let mut clock = Clock::new();
        clock.seek(Duration::from_secs(10));
        clock.play();
        std::thread::sleep(Duration::from_millis(10));

        clock.set_rate(2.0);
        assert!(clock.position() >= Duration::from_secs(10));
        assert_eq!(clock.rate(), 2.0);

        clock.set_rate(0.0);
        assert!(clock.rate() > 0.0, "zero rate is what pause is for");
    }

    #[test]
    fn a_frame_due_now_is_presented() {
        let mut pacing = Pacing::for_frame_rate(24.0);

        let action = pacing.decide(Duration::from_millis(100), Duration::from_millis(100));

        assert_eq!(action, Action::Present);
        assert_eq!(pacing.counts(), (1, 0, 0));
    }

    #[test]
    fn a_frame_from_the_future_asks_the_caller_to_wait() {
        let mut pacing = Pacing::with_tolerance(Duration::from_millis(5));

        let action = pacing.decide(Duration::from_millis(200), Duration::from_millis(100));

        assert_eq!(action, Action::Wait(Duration::from_millis(95)));
        assert_eq!(pacing.counts(), (0, 0, 0), "waiting is not presenting");
    }

    /// Lag that never recovers is the failure mode: a player that draws every
    /// frame however late falls further behind for the rest of the file.
    #[test]
    fn a_frame_too_late_to_matter_is_dropped() {
        let mut pacing = Pacing::for_frame_rate(24.0);

        let action = pacing.decide(Duration::from_millis(100), Duration::from_millis(500));

        assert_eq!(action, Action::Drop);
        assert_eq!(pacing.counts(), (0, 1, 0));
        assert_eq!(pacing.drop_rate(), 1.0);
    }

    #[test]
    fn a_frame_barely_early_is_presented_rather_than_waited_for() {
        let mut pacing = Pacing::for_frame_rate(24.0);

        // A millisecond early, against a ~20 ms tolerance.
        let action = pacing.decide(Duration::from_millis(101), Duration::from_millis(100));

        assert_eq!(action, Action::Present);
    }

    #[test]
    fn counters_survive_until_they_are_reset() {
        let mut pacing = Pacing::for_frame_rate(24.0);

        pacing.decide(Duration::ZERO, Duration::ZERO);
        pacing.decide(Duration::ZERO, Duration::from_secs(1));
        pacing.note_repeat();

        assert_eq!(pacing.counts(), (1, 1, 1));
        assert!((pacing.drop_rate() - 0.5).abs() < 1e-9);

        pacing.reset();
        assert_eq!(pacing.counts(), (0, 0, 0));
        assert_eq!(pacing.drop_rate(), 0.0);
    }

    /// Video chases audio, which is the whole point of the trait.
    #[test]
    fn pacing_follows_whatever_master_it_is_given() {
        let master = FixedClock(Duration::from_millis(500));
        let mut pacing = Pacing::for_frame_rate(24.0);

        assert_eq!(
            pacing.decide(Duration::from_millis(500), master.position()),
            Action::Present
        );
        assert_eq!(
            pacing.decide(Duration::from_millis(100), master.position()),
            Action::Drop
        );
        assert!(master.is_running());
    }
}
