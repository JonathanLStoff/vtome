//! MP4 timestamps, against a file with B-frames.
//!
//! The `mp4` crate's `start_time` is the decode time and `ctts` is added to it
//! to get presentation time. Read the other way round, PTS and DTS swap, which
//! no test notices until a decoder reorders by PTS and shows B-frames early.
//! `tests/data/bars_h264.mp4` is I P B B P B B… in decode order.

#![cfg(feature = "demux")]

use std::time::Duration;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/bars_h264.mp4");

fn video_packets() -> Vec<vtome::Packet> {
    let mut demuxer = vtome::open_media(FIXTURE).unwrap();
    let track = demuxer.info().video().unwrap().id;
    let mut packets = Vec::new();

    while let Some(packet) = demuxer.next_packet().unwrap() {
        if packet.track_id == track {
            packets.push(packet);
        }
    }

    packets
}

#[test]
fn packets_arrive_in_decode_order_with_rising_decode_times() {
    let packets = video_packets();

    assert_eq!(packets.len(), 24);
    assert!(packets.windows(2).all(|pair| pair[0].dts < pair[1].dts));
}

/// Every sample in this file carries a non-negative composition offset, so no
/// picture is shown before it is decoded — and the reference frames are shown
/// strictly after.
#[test]
fn no_picture_is_shown_before_it_is_decoded() {
    let packets = video_packets();

    assert!(packets.iter().all(|packet| packet.pts >= packet.dts));
    assert!(packets.iter().any(|packet| packet.pts > packet.dts));
}

/// The P-frame that follows the I-frame in the file is shown after the two
/// B-frames decoded behind it.
#[test]
fn b_frames_are_shown_before_the_reference_decoded_ahead_of_them() {
    let packets = video_packets();

    assert!(packets[0].is_keyframe);
    assert!(packets[1].pts > packets[2].pts, "{:?}", &packets[..4]);
    assert!(packets[1].pts > packets[3].pts);
}

#[test]
fn in_presentation_order_the_frames_are_evenly_spaced() {
    let mut presented: Vec<Duration> = video_packets().iter().map(|packet| packet.pts).collect();
    presented.sort_unstable();

    let frame = Duration::from_secs(1) / 24;

    for pair in presented.windows(2) {
        assert!((pair[1] - pair[0]).abs_diff(frame) < Duration::from_millis(1));
    }
}
