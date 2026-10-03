//! A clip's soundtrack, played through atome in step with its pictures.
//!
//! vtome never opens an audio device. With [`Audio::atome`](crate::Audio), the
//! application hands over an atome output's [`Scheduler`], and every video clip
//! that has sound — its own audio track, or an audio file given with
//! [`Clip::audio`](crate::Clip::audio) when the film has none — is decoded by
//! atome and scheduled on that output at exactly the point on its clock where
//! the clip's first picture goes up. The pictures follow the same clock, so the
//! two cannot drift.
//!
//! The soundtrack is fed a block at a time from a thread of its own, never more
//! than [`LEAD`] ahead of what the speakers are playing, as one atome *voice*:
//! stopping the clip cancels the voice, and whatever of it has not reached the
//! device is taken back — silence within a buffer, not a second later.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use atome::output::Scheduler;

use crate::error::{Error, Result};

/// How far ahead of the speakers a soundtrack is scheduled. Enough to ride out
/// a busy moment on the feeding thread; little enough that a cancel has little
/// to take back.
pub const LEAD: Duration = Duration::from_millis(750);

/// Frames decoded and scheduled at a time.
const BLOCK_FRAMES: usize = 4096;

/// A soundtrack being fed to atome. Stopped when dropped.
pub(crate) struct Soundtrack {
    voice: u64,
    scheduler: Scheduler<f32>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Soundtrack {
    /// Starts feeding `path`'s audio so its first sample is heard at `at` on
    /// the output's clock, starting over every `period` if the film loops.
    ///
    /// # Errors
    ///
    /// Whatever atome refuses about the file — no audio in it, an encoding it
    /// does not decode — at once, rather than as silence later.
    pub(crate) fn start(
        path: &Path,
        scheduler: Scheduler<f32>,
        at: Duration,
        period: Option<Duration>,
    ) -> Result<Self> {
        // Opened here, on the caller's thread, so a file atome cannot read is
        // an error now. The feeder opens its own for each pass.
        open(path)?;

        let voice = scheduler.new_voice();
        let stop = Arc::new(AtomicBool::new(false));

        let thread = {
            let (path, scheduler, stop) = (path.to_path_buf(), scheduler.clone(), Arc::clone(&stop));

            std::thread::Builder::new()
                .name("vtome-soundtrack".to_string())
                .spawn(move || feed(&path, &scheduler, voice, at, period, &stop))
                .map_err(|error| Error::unsupported(format!("no thread for a soundtrack: {error}")))?
        };

        Ok(Soundtrack {
            voice,
            scheduler,
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Soundtrack {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.scheduler.cancel(self.voice);

        // The feeder notices within one of its short sleeps; it is never
        // blocked on anything longer.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Whether atome can play `path`'s audio, and the stream if so.
fn open(path: &Path) -> Result<Box<dyn atome::import::AudioStream<f32>>> {
    let encoding = atome::import::find_type(path).map_err(|error| no_audio(path, error))?;
    atome::import::stream_as::<f32>(path, encoding).map_err(|error| no_audio(path, error))
}

fn no_audio(path: &Path, error: impl std::fmt::Display) -> Error {
    Error::unsupported(format!("{}: atome cannot play its audio: {error}", path.display()))
}

/// The feeding loop: decode, fit to the output, schedule, never more than
/// [`LEAD`] ahead — and again from the top each `period` for a looping film.
fn feed(
    path: &PathBuf,
    scheduler: &Scheduler<f32>,
    voice: u64,
    at: Duration,
    period: Option<Duration>,
    stop: &AtomicBool,
) {
    let channels = usize::from(scheduler.channels().max(1));
    let rate = f64::from(scheduler.sample_rate());
    let index_at = |time: Duration| (time.as_secs_f64() * rate).round() as usize * channels;
    let lead = index_at(LEAD);

    let mut pass = 0_u32;

    loop {
        let start = at + period.map_or(Duration::ZERO, |period| period * pass);
        let mut index = index_at(start);

        let Ok(mut stream) = open(path) else {
            return;
        };

        let source_channels = usize::from(stream.channels().max(1));
        let source_rate = stream.sample_rate();
        let mut block = vec![0.0_f32; BLOCK_FRAMES * source_channels];

        loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }

            // Held back until the speakers are within `LEAD` of it.
            let playing = scheduler.clock().frames() as usize * channels;
            if index > playing + lead {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }

            let read = match stream.read(&mut block) {
                Ok(0) | Err(_) => break,
                Ok(read) => read - read % source_channels,
            };

            let samples = scheduler.align(&block[..read], source_rate, stream.channels());

            // A full command queue means the mixer is behind; wait for it
            // rather than drop audio.
            loop {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                match scheduler.schedule(&samples, index, Some(voice)) {
                    Ok(next) => {
                        index = next;
                        break;
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(2)),
                }
            }
        }

        match period {
            Some(period) if !period.is_zero() => pass += 1,
            _ => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atome::output::{OutputClass, OutputType, SampleRate};

    const SILENT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/bars_h264.mp4");

    /// An output to schedule into, or `None` on a machine with no device —
    /// the stream is never built, so nothing is heard.
    fn output() -> Option<OutputClass<f32>> {
        let device = atome::output::default_device()?;
        Some(OutputClass::new(Some(device), OutputType::CoreAudio, 2, SampleRate::Hz48k, Some(512)))
    }

    #[test]
    fn a_film_without_sound_is_refused_up_front() {
        let Some(output) = output() else {
            eprintln!("skipping: no output device");
            return;
        };

        let result = Soundtrack::start(Path::new(SILENT), output.scheduler(), Duration::ZERO, None);
        assert!(matches!(result, Err(Error::Unsupported { .. })), "the fixture has no audio");
    }

    /// Made with ffmpeg, since vtome writes no audio; skipped without it.
    #[test]
    fn a_soundtrack_feeds_ahead_and_stops_when_dropped() {
        let Some(output) = output() else {
            eprintln!("skipping: no output device");
            return;
        };

        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("tone.wav");
        let made = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi"])
            .args(["-i", "sine=frequency=440:sample_rate=44100:d=5"])
            .arg(&path)
            .status();
        if !made.is_ok_and(|status| status.success()) {
            eprintln!("skipping: ffmpeg is needed to make a sound file");
            return;
        }

        let soundtrack =
            Soundtrack::start(&path, output.scheduler(), Duration::from_millis(100), None).unwrap();
        std::thread::sleep(Duration::from_millis(50));

        // With the stream never built the clock stands at zero, so the feeder
        // stays within its lead and is waiting; dropping it returns at once.
        let stopping = std::time::Instant::now();
        drop(soundtrack);
        assert!(stopping.elapsed() < Duration::from_millis(500));
    }
}
