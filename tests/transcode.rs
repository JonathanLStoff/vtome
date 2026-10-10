//! Transcoding, end to end, against the H.264 fixture.
//!
//! `tests/data/bars_h264.mp4` is 24 frames of 128×96 with B-frames and a bar
//! that moves four pixels a frame (see `make_h264_fixture.sh`), so a file
//! written from it can be decoded again and checked frame by frame: the same
//! number of pictures, in the same order, with the bar where it was.
//!
//! The files are checked against `planning/TODO.md` §15's spec from their own
//! bytes — the SPS says High 4.1 and no reordering, the MP4 has its index
//! first. Where `ffprobe` is installed it is asked too, as a reader that is not
//! vtome's; where it is not, that half is skipped. vtome itself never runs it.
//!
//! ```text
//! cargo test --features transcode --test transcode
//! ```

#![cfg(all(target_vendor = "apple", feature = "transcode"))]

use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::AtomicBool;

use vtome::bitstream::{AvcConfig, SequenceParameterSet};
use vtome::color::Matrix;
use vtome::transcode::{transcode, transcode_into, Settings};
use vtome::{Container, Encoding, Frame, Hardware, VideoSource};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/bars_h264.mp4");
const FRAMES: u64 = 24;

/// Where the moving bar's centre is, from the luma of one row in the bottom
/// half — scaled to the frame, so a shrunk copy reads the same.
fn bar_position(frame: &Frame) -> f64 {
    let row = frame.row(0, frame.height() * 3 / 4).unwrap();
    let lit: Vec<usize> = (0..row.len()).filter(|&x| row[x] > 128).collect();

    assert!(!lit.is_empty(), "no bar in the frame at {:?}", frame.pts());

    lit.iter().sum::<usize>() as f64 / lit.len() as f64 / f64::from(frame.width())
}

/// Every frame of a file, in display order.
fn decode_all(path: &Path) -> Vec<Frame> {
    let mut source = VideoSource::from_file(path).expect("the written file opens");
    let mut frames = Vec::new();

    loop {
        source.advance(std::time::Duration::ZERO).unwrap();
        match source.pop_frame() {
            Some(frame) => frames.push(frame),
            None if source.is_finished() => break,
            None => {}
        }
    }

    frames
}

/// The top-level box types of an MP4, in order.
fn top_boxes(path: &Path) -> Vec<String> {
    let bytes = std::fs::read(path).unwrap();
    let mut boxes = Vec::new();
    let mut offset = 0;

    while offset + 8 <= bytes.len() {
        let size = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        boxes.push(String::from_utf8_lossy(&bytes[offset + 4..offset + 8]).into_owned());
        if size < 8 {
            break;
        }
        offset += size;
    }

    boxes
}

/// `ffprobe`'s view of the first video stream, or `None` without ffprobe.
fn ffprobe(path: &Path, entries: &str) -> Option<String> {
    let output = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", entries])
        .args(["-of", "default=noprint_wrappers=1"])
        .arg(path)
        .output()
        .ok()?;

    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

#[test]
fn h264_is_written_to_the_spec_and_plays_back_frame_for_frame() {
    let directory = tempfile::TempDir::new().unwrap();
    let output = directory.path().join("bars.mp4");
    let mut reports = Vec::new();

    let summary = transcode(
        FIXTURE,
        &output,
        &Settings::default(),
        |progress| reports.push(progress),
        &AtomicBool::new(false),
    )
    .expect("the fixture transcodes");

    assert_eq!(summary.encoding, Encoding::H264, "H.264 is the default");
    assert_eq!(summary.container, Container::Mp4);
    assert_eq!((summary.width, summary.height), (128, 96));
    assert_eq!(summary.frame_rate.as_f64().round(), 24.0);
    assert_eq!(summary.bit_depth, 8);
    assert_eq!(summary.frames, FRAMES);
    assert!(summary.duration.as_millis().abs_diff(1000) <= 1, "{:?}", summary.duration);

    assert_eq!(reports.len() as u64, FRAMES + 1, "one per picture, and the end");
    assert!(reports.windows(2).all(|pair| pair[1].fraction >= pair[0].fraction));
    assert_eq!(reports.last().unwrap().fraction, 1.0);

    // Faststart: the index before the media.
    let boxes = top_boxes(&output);
    let moov = boxes.iter().position(|kind| kind == "moov").unwrap();
    let mdat = boxes.iter().position(|kind| kind == "mdat").unwrap();
    assert!(moov < mdat, "moov must come first: {boxes:?}");

    // The SPS says what §15 asks, and the colour came through.
    let demuxer = vtome::open_media(&output).unwrap();
    let track = demuxer.info().video().unwrap().clone();
    let avc = AvcConfig::parse(&track.extra_data).unwrap();
    let sps = SequenceParameterSet::parse(&avc.sequence_parameter_sets[0]).unwrap();

    assert_eq!((sps.profile_idc, sps.level_idc), (100, 41), "High 4.1");
    assert_eq!((sps.bit_depth, sps.chroma_format_idc), (8, 1), "8-bit 4:2:0");
    assert_eq!(sps.max_num_reorder_frames.unwrap_or(0), 0, "no B-frames");
    assert_eq!(track.color.matrix, Matrix::Bt601, "the fixture is BT.601");

    // And it plays: every picture, in order, the bar where it was.
    let original = decode_all(Path::new(FIXTURE));
    let written = decode_all(&output);
    assert_eq!(written.len(), original.len());

    for (index, (before, after)) in original.iter().zip(&written).enumerate() {
        let (before, after) = (bar_position(before), bar_position(after));
        assert!(
            (before - after).abs() < 0.02,
            "frame {index}: the bar moved from {before:.3} to {after:.3}"
        );
    }

    if let Some(probe) = ffprobe(&output, "stream=codec_name,profile,level,has_b_frames,pix_fmt") {
        assert!(probe.contains("codec_name=h264"), "{probe}");
        assert!(probe.contains("profile=High"), "{probe}");
        assert!(probe.contains("level=41"), "{probe}");
        assert!(probe.contains("has_b_frames=0"), "{probe}");
        assert!(probe.contains("pix_fmt=yuv420p"), "{probe}");
    }
}

/// The proxy path: shrunk, low quality, the same spec.
#[test]
fn a_shrunk_copy_keeps_its_shape_and_its_pictures() {
    let directory = tempfile::TempDir::new().unwrap();
    let output = directory.path().join("small.mp4");

    let settings = Settings {
        max_size: Some((64, 64)),
        ..Settings::proxy()
    };

    let summary = transcode(FIXTURE, &output, &settings, |_| {}, &AtomicBool::new(false)).unwrap();
    assert_eq!((summary.width, summary.height), (64, 48));

    let written = decode_all(&output);
    assert_eq!(written.len() as u64, FRAMES);
    assert_eq!((written[0].width(), written[0].height()), (64, 48));

    let positions: Vec<f64> = written.iter().map(bar_position).collect();
    assert!(positions.windows(2).all(|pair| pair[1] > pair[0]), "{positions:?}");

    // Guessed by size, a 64×48 picture would be BT.601 anyway; what matters is
    // that the SPS said so and the demuxer read it rather than guessing.
    let demuxer = vtome::open_media(&output).unwrap();
    let track = demuxer.info().video().unwrap();
    let avc = AvcConfig::parse(&track.extra_data).unwrap();
    let sps = SequenceParameterSet::parse(&avc.sequence_parameter_sets[0]).unwrap();
    assert_eq!(sps.matrix, Some(6));
}

/// AV1 through rav1e into WebM: vtome reads its own WebM back — track,
/// colour, frame rate, keyframes, Cues — and so does ffprobe.
#[test]
fn av1_goes_into_a_webm_that_reads_back() {
    let directory = tempfile::TempDir::new().unwrap();
    let output = directory.path().join("bars.webm");

    let settings = Settings {
        encoding: Some(Encoding::Av1),
        realtime: true,
        ..Settings::default()
    };

    let summary = transcode(FIXTURE, &output, &settings, |_| {}, &AtomicBool::new(false)).unwrap();
    assert_eq!((summary.encoding, summary.container), (Encoding::Av1, Container::WebM));
    assert_eq!((summary.frame_rate.as_f64().round(), summary.bit_depth), (24.0, 8));
    assert_eq!(summary.frames, FRAMES);
    assert!(!summary.encoder_hardware);

    let mut demuxer = vtome::open_media(&output).unwrap();
    let info = demuxer.info().clone();
    let track = info.video().unwrap();

    assert_eq!(info.container, Container::WebM);
    assert_eq!(track.encoding, Some(Encoding::Av1));
    assert_eq!((track.width, track.height), (128, 96));
    assert_eq!(track.color.matrix, Matrix::Bt601, "Colour, written and read");
    assert_eq!(track.frame_rate.map(|rate| rate.as_f64().round()), Some(24.0));
    assert!(info.duration.as_millis().abs_diff(1000) <= 2, "{:?}", info.duration);
    assert_eq!(track.extra_data[0], 0x81, "av1C as CodecPrivate");

    let mut packets = Vec::new();
    while let Some(packet) = demuxer.next_packet().unwrap() {
        packets.push(packet);
    }
    assert_eq!(packets.len() as u64, FRAMES);
    assert!(packets[0].is_keyframe);
    assert!(packets[1..].iter().all(|packet| !packet.is_keyframe), "one second, one keyframe");

    // Seeking goes through the Cues.
    demuxer.seek(std::time::Duration::from_millis(500)).unwrap();
    assert!(demuxer.next_packet().unwrap().is_some());

    if let Some(probe) = ffprobe(&output, "stream=codec_name,pix_fmt,color_space,nb_read_frames") {
        assert!(probe.contains("codec_name=av1"), "{probe}");
        assert!(probe.contains("pix_fmt=yuv420p"), "{probe}");
    }
}

#[test]
fn hardware_can_be_ruled_out_end_to_end() {
    let directory = tempfile::TempDir::new().unwrap();
    let output = directory.path().join("software.mp4");

    let settings = Settings {
        hardware: Hardware::Off,
        ..Settings::proxy()
    };

    let summary = transcode(FIXTURE, &output, &settings, |_| {}, &AtomicBool::new(false)).unwrap();
    assert!(!summary.decoder_hardware && !summary.encoder_hardware);
    assert_eq!(decode_all(&output).len() as u64, FRAMES);
}

#[test]
fn a_cancelled_transcode_stops_and_leaves_nothing_behind() {
    let directory = tempfile::TempDir::new().unwrap();
    let output = directory.path().join("never.mp4");

    let result = transcode(FIXTURE, &output, &Settings::default(), |_| {}, &AtomicBool::new(true));

    assert!(matches!(result, Err(vtome::Error::Cancelled)), "{result:?}");
    assert!(!output.exists());
}

#[test]
fn an_input_that_will_not_open_writes_nothing() {
    let directory = tempfile::TempDir::new().unwrap();
    let output = directory.path().join("never.mp4");

    let result = transcode("/no/such/file.mp4", &output, &Settings::default(), |_| {}, &AtomicBool::new(false));

    assert!(result.is_err());
    assert!(!output.exists());
}

/// The same transcode into a `Cursor` as into a path: the MP4 is rearranged in
/// the destination itself, and the result plays back as the path's does.
#[test]
fn h264_into_a_writer_is_faststart_and_plays_back_like_the_path() {
    let directory = tempfile::TempDir::new().unwrap();
    let by_path = directory.path().join("path.mp4");
    let by_writer = directory.path().join("writer.mp4");

    let path_summary =
        transcode(FIXTURE, &by_path, &Settings::default(), |_| {}, &AtomicBool::new(false)).unwrap();

    let mut sink = Cursor::new(Vec::new());
    let summary = transcode_into(
        FIXTURE,
        &mut sink,
        &Settings::default(),
        |_| {},
        &AtomicBool::new(false),
    )
    .expect("the fixture transcodes into a writer");

    assert_eq!(summary.bytes, sink.get_ref().len() as u64);
    assert_eq!(sink.position(), summary.bytes, "left at the end");
    assert_eq!(
        (summary.frames, summary.encoding, summary.container),
        (path_summary.frames, path_summary.encoding, path_summary.container)
    );

    std::fs::write(&by_writer, sink.get_ref()).unwrap();

    let boxes = top_boxes(&by_writer);
    assert_eq!(boxes, top_boxes(&by_path), "the same layout, index first");
    assert!(boxes.iter().position(|kind| kind == "moov") < boxes.iter().position(|kind| kind == "mdat"));

    let original = decode_all(Path::new(FIXTURE));
    let written = decode_all(&by_writer);
    assert_eq!(written.len(), original.len());

    for (index, (before, after)) in original.iter().zip(&written).enumerate() {
        let (before, after) = (bar_position(before), bar_position(after));
        assert!(
            (before - after).abs() < 0.02,
            "frame {index}: the bar moved from {before:.3} to {after:.3}"
        );
    }
}

#[test]
fn av1_into_a_writer_is_a_webm_that_reads_back() {
    let directory = tempfile::TempDir::new().unwrap();
    let output = directory.path().join("writer.webm");

    let settings = Settings {
        encoding: Some(Encoding::Av1),
        realtime: true,
        ..Settings::default()
    };

    let mut sink = Cursor::new(Vec::new());
    let summary = transcode_into(FIXTURE, &mut sink, &settings, |_| {}, &AtomicBool::new(false)).unwrap();

    assert_eq!((summary.encoding, summary.container), (Encoding::Av1, Container::WebM));
    assert_eq!(summary.bytes, sink.get_ref().len() as u64);

    std::fs::write(&output, sink.get_ref()).unwrap();

    let mut demuxer = vtome::open_media(&output).unwrap();
    assert_eq!(demuxer.info().container, Container::WebM);

    let mut packets = 0;
    while demuxer.next_packet().unwrap().is_some() {
        packets += 1;
    }
    assert_eq!(packets, FRAMES);

    // The patched slots — segment size, Cues position, duration — are what let
    // it seek, and they were patched in the writer rather than in a file.
    demuxer.seek(std::time::Duration::from_millis(500)).unwrap();
    assert!(demuxer.next_packet().unwrap().is_some());
}

#[test]
fn a_writer_that_is_not_at_its_start_is_refused() {
    let mut sink = Cursor::new(vec![0_u8; 16]);
    sink.seek(SeekFrom::End(0)).unwrap();

    let result = transcode_into(FIXTURE, &mut sink, &Settings::default(), |_| {}, &AtomicBool::new(false));

    assert!(result.is_err(), "a file cannot begin partway into a stream");
    assert_eq!(sink.get_ref().len(), 16, "nothing was written over what was there");

    let mut rewound = Vec::new();
    sink.seek(SeekFrom::Start(0)).unwrap();
    sink.read_to_end(&mut rewound).unwrap();
    assert_eq!(rewound, vec![0_u8; 16]);
}
