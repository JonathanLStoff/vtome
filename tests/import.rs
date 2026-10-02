//! The import queue, end to end: a proxy at the output path first, the final
//! file in its place later, never more than one encoder per lane, and every
//! file's progress in one list.
//!
//! ```text
//! cargo test --features transcode --test import
//! cargo test --features split-audio --test import    # and the FLAC split
//! ```

#![cfg(all(target_vendor = "apple", feature = "transcode"))]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use vtome::{
    Encoding, Frame, Hardware, ImportOptions, ImportQueue, QueueConfig, Stage, VideoSource,
};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/bars_h264.mp4");

fn decode_all(path: &Path) -> Vec<Frame> {
    let mut source = VideoSource::from_file(path).expect("the output opens");
    let mut frames = Vec::new();

    loop {
        source.advance(Duration::ZERO).unwrap();
        match source.pop_frame() {
            Some(frame) => frames.push(frame),
            None if source.is_finished() => break,
            None => {}
        }
    }

    frames
}

/// Several copies of the fixture under different names, so a queue has more
/// than one file to work through.
fn inputs(directory: &Path, count: usize) -> Vec<PathBuf> {
    (0..count)
        .map(|index| {
            let path = directory.join(format!("input-{index}.mp4"));
            std::fs::copy(FIXTURE, &path).unwrap();
            path
        })
        .collect()
}

#[test]
fn the_output_plays_from_the_moment_it_is_ready_and_ends_as_the_final_file() {
    let directory = tempfile::TempDir::new().unwrap();
    let output = directory.path().join("bars.mp4");
    let queue = ImportQueue::new(QueueConfig::default());

    let id = queue
        .import(FIXTURE, &output, None, ImportOptions::default())
        .expect("the fixture is accepted");

    // As soon as the queue says the path is ready, it plays — whichever of
    // the proxy or the final file is there by then.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let progress = queue.job(id).unwrap();
        if progress.output_ready {
            assert_eq!(decode_all(&output).len(), 24);
            break;
        }
        assert!(!progress.stage.is_over(), "ended without a proxy: {progress:?}");
        assert!(Instant::now() < deadline, "no proxy within a minute");
        std::thread::sleep(Duration::from_millis(5));
    }

    let done = queue.wait(id).unwrap();
    assert_eq!(done.stage, Stage::Done, "{:?}", done.error);
    assert!(done.final_ready && done.output_ready);
    assert_eq!(done.fraction, 1.0);
    assert_eq!(done.encoding, Encoding::H264);
    assert_eq!(done.size, (128, 96));

    // The final file, at full size, in place of the proxy — and nothing left
    // beside it.
    let frames = decode_all(&output);
    assert_eq!(frames.len(), 24);
    assert_eq!((frames[0].width(), frames[0].height()), (128, 96));

    let leftovers: Vec<_> = std::fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().contains("vtome"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    assert_eq!(queue.progress().len(), 1);
    assert_eq!(queue.clear_finished().len(), 1);
    assert!(queue.progress().is_empty());
}

/// A queue, not a thread per file: however many are waiting, one proxy and
/// one final encode run at a time, and every file is reported.
#[test]
fn many_files_go_through_one_encoder_per_lane() {
    let directory = tempfile::TempDir::new().unwrap();
    let queue = ImportQueue::new(QueueConfig::default());

    let ids: Vec<_> = inputs(directory.path(), 5)
        .iter()
        .enumerate()
        .map(|(index, input)| {
            let output = directory.path().join(format!("output-{index}.mp4"));
            queue.import(input, output, None, ImportOptions::default()).unwrap()
        })
        .collect();

    let (mut most_proxies, mut most_finals) = (0, 0);
    let deadline = Instant::now() + Duration::from_secs(120);

    loop {
        let all = queue.progress();
        assert_eq!(all.len(), ids.len(), "every file is reported");

        let count = |stages: &[Stage]| all.iter().filter(|job| stages.contains(&job.stage)).count();
        most_proxies = most_proxies.max(count(&[Stage::Proxy, Stage::Audio]));
        most_finals = most_finals.max(count(&[Stage::Final, Stage::Replacing]));

        if all.iter().all(|job| job.stage.is_over()) {
            break;
        }

        assert!(Instant::now() < deadline, "the queue did not drain: {all:?}");
        std::thread::sleep(Duration::from_millis(2));
    }

    assert!(most_proxies <= 1, "{most_proxies} proxies at once");
    assert!(most_finals <= 1, "{most_finals} final encodes at once");

    for progress in queue.progress() {
        assert_eq!(progress.stage, Stage::Done, "{:?}", progress.error);
        assert_eq!(decode_all(&progress.output).len(), 24);
    }
}

#[test]
fn a_cancelled_import_stops_and_cleans_up() {
    let directory = tempfile::TempDir::new().unwrap();
    let temp = directory.path().join("work");
    std::fs::create_dir(&temp).unwrap();

    let queue = ImportQueue::new(QueueConfig::default());

    // Queued behind another file, so it is still waiting when cancelled.
    let inputs = inputs(directory.path(), 2);
    let options = ImportOptions {
        temp_dir: Some(temp.clone()),
        ..ImportOptions::default()
    };

    let first = queue
        .import(&inputs[0], directory.path().join("first.mp4"), None, options.clone())
        .unwrap();
    let second = queue
        .import(&inputs[1], directory.path().join("second.mp4"), None, options)
        .unwrap();

    assert!(queue.cancel(second));
    assert_eq!(queue.wait(second).unwrap().stage, Stage::Cancelled);
    assert!(!queue.cancel(second), "already over");

    assert_eq!(queue.wait(first).unwrap().stage, Stage::Done);

    // The first file's temp directory went when it finished; the second
    // never made one.
    assert_eq!(std::fs::read_dir(&temp).unwrap().count(), 0);
}

/// Hardware is an option: off, both halves run in software.
#[test]
fn hardware_acceleration_is_an_import_option() {
    let directory = tempfile::TempDir::new().unwrap();
    let queue = ImportQueue::new(QueueConfig::default());

    let options = ImportOptions {
        hardware: Hardware::Off,
        ..ImportOptions::default()
    };

    let id = queue
        .import(FIXTURE, directory.path().join("software.mp4"), None, options)
        .unwrap();

    let done = queue.wait(id).unwrap();
    assert_eq!(done.stage, Stage::Done, "{:?}", done.error);
    assert_eq!(done.decoder_hardware, Some(false));
    assert_eq!(done.encoder_hardware, Some(false));

    // AV1 is software, so requiring hardware of it is refused at the call.
    let refused = queue.import(
        FIXTURE,
        directory.path().join("never.webm"),
        None,
        ImportOptions {
            encoding: Some(Encoding::Av1),
            hardware: Hardware::Require,
            ..ImportOptions::default()
        },
    );
    assert!(matches!(refused, Err(vtome::Error::NoEncoder { .. })), "{refused:?}");
}

/// A bad import is an error from the call, not a failed job later.
#[test]
fn what_cannot_work_is_refused_before_it_is_queued() {
    let directory = tempfile::TempDir::new().unwrap();
    let queue = ImportQueue::new(QueueConfig::default());
    let output = directory.path().join("out.mp4");

    assert!(queue
        .import("/no/such/file.mp4", &output, None, ImportOptions::default())
        .is_err());

    assert!(queue
        .import(FIXTURE, "/no/such/directory/out.mp4", None, ImportOptions::default())
        .is_err());

    // The fixture has no soundtrack to split out.
    let audio = queue.import(
        FIXTURE,
        &output,
        Some(directory.path().join("out.flac")),
        ImportOptions::default(),
    );
    assert!(audio.is_err(), "{audio:?}");

    assert!(queue.progress().is_empty(), "nothing was queued");
}

/// The soundtrack to FLAC, through atome. Needs ffmpeg to make a film with
/// sound in it, since vtome never writes audio; skipped without it.
#[cfg(feature = "split-audio")]
#[test]
fn the_audio_is_split_out_to_flac() {
    let directory = tempfile::TempDir::new().unwrap();
    let input = directory.path().join("with-sound.mp4");

    let made = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", "testsrc=s=160x120:r=24:d=1"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:d=1"])
        .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest"])
        .arg(&input)
        .status();

    if !made.is_ok_and(|status| status.success()) {
        eprintln!("skipping: ffmpeg is needed to make a film with sound");
        return;
    }

    let queue = ImportQueue::new(QueueConfig::default());
    let flac = directory.path().join("with-sound.flac");

    let id = queue
        .import(&input, directory.path().join("with-sound-out.mp4"), Some(flac.clone()), ImportOptions::default())
        .unwrap();

    let done = queue.wait(id).unwrap();
    assert_eq!(done.stage, Stage::Done, "{:?}", done.error);

    let bytes = std::fs::read(&flac).unwrap();
    assert_eq!(&bytes[..4], b"fLaC");
}
