//! A `.mov` as QuickTime writes it — AAC in a version 1 sound description,
//! its `esds` inside `wave` — opens, with its audio track intact.

#![cfg(feature = "demux")]

use vtome::media::TrackKind;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/aac_quicktime_v1.mov");

#[test]
fn a_quicktime_sound_description_opens() {
    let mut media = vtome::open_media(FIXTURE).expect("open the .mov");

    let audio = media
        .info()
        .tracks
        .iter()
        .find(|track| track.kind == TrackKind::Audio)
        .expect("an audio track")
        .clone();
    assert_eq!(audio.codec_id, "mp4a");

    let mut packets = 0;
    while let Some(packet) = media.next_packet().expect("read a packet") {
        if packet.track_id == audio.id {
            assert!(!packet.data.is_empty());
            packets += 1;
        }
    }
    assert!(packets > 0, "the audio's samples are read");
}

/// What an import does with it: the picture transcodes rather than the file
/// being refused at the door.
#[cfg(feature = "transcode")]
#[test]
fn a_quicktime_movie_transcodes() {
    use std::sync::atomic::AtomicBool;
    use vtome::transcode::{transcode, Settings};

    let directory = tempfile::TempDir::new().unwrap();
    let output = directory.path().join("icon.mp4");

    let summary = transcode(FIXTURE, &output, &Settings::default(), |_| {}, &AtomicBool::new(false))
        .expect("the .mov transcodes");

    assert!(summary.frames > 0);
    assert!(vtome::open_media(&output).is_ok());
}
