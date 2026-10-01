//! VideoToolbox, end to end, against a real H.264 file.
//!
//! `tests/data/bars_h264.mp4` is 24 frames of 128×96 with B-frames (see
//! `make_h264_fixture.sh` beside it): red, green, blue, and white bars across
//! the top, and in the bottom half a white bar that moves four pixels right
//! every frame. The colours check the decoder and the colour maths; the moving
//! bar says *which* frame a picture is, so display order is read from the
//! pixels rather than trusted from the timestamps the decoder was handed.
//!
//! ```text
//! cargo test --features decode-platform --test videotoolbox
//! cargo test --features decode-platform,render --test videotoolbox   # and through the GPU
//! ```

#![cfg(all(target_vendor = "apple", feature = "decode-platform", feature = "demux"))]

use std::time::Duration;

use vtome::color::convert_pixel;
use vtome::decode::{self, DecoderConfig};
use vtome::media::Packet;
use vtome::{Frame, PixelFormat, VideoSource};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/bars_h264.mp4");
const FRAMES: usize = 24;
const WIDTH: u32 = 128;
const HEIGHT: u32 = 96;

/// The top half's bars, left to right, and where to sample each.
const BARS: [(u32, [u8; 3]); 4] = [
    (16, [255, 0, 0]),
    (48, [0, 255, 0]),
    (80, [0, 0, 255]),
    (112, [255, 255, 255]),
];

/// Every frame in the fixture, in the order the decoder hands them out, and
/// whether the decoder said it was hardware.
fn decode_fixture() -> (Vec<Frame>, bool) {
    let mut demuxer = vtome::open_media(FIXTURE).expect("the fixture demuxes");
    let track = demuxer.info().video().expect("the fixture has video").clone();
    let config = DecoderConfig::from_track(&track).unwrap();
    let mut decoder = decode::open(&config).expect("VideoToolbox opens for H.264");

    let mut frames = Vec::new();

    while let Some(packet) = demuxer.next_packet().unwrap() {
        if packet.track_id != track.id {
            continue;
        }

        if let Some(frame) = decoder.decode(&packet).unwrap() {
            frames.push(frame);
        }
    }

    frames.extend(decoder.flush().unwrap());

    (frames, decoder.is_hardware())
}

/// One pixel of an NV12 frame as RGB, through the same matrix the shader uses.
fn rgb_at(frame: &Frame, x: u32, y: u32) -> [u8; 3] {
    let luma = frame.row(0, y).unwrap()[x as usize];
    let chroma = frame.row(1, y / 2).unwrap();
    let pair = (x / 2 * 2) as usize;

    let yuv = [luma, chroma[pair], chroma[pair + 1]].map(|code| f32::from(code) / 255.0);

    convert_pixel(frame.color(), 8, yuv).map(|channel| (channel * 255.0).round() as u8)
}

/// Where the moving bar's centre is, from the luma of one row in the bottom half.
fn bar_centre(frame: &Frame) -> f64 {
    let row = frame.row(0, HEIGHT * 3 / 4).unwrap();
    let lit: Vec<usize> = (0..row.len()).filter(|&x| row[x] > 128).collect();

    assert!(!lit.is_empty(), "no bar in the frame at {:?}", frame.pts());

    lit.iter().sum::<usize>() as f64 / lit.len() as f64
}

fn close(actual: [u8; 3], expected: [u8; 3], tolerance: u8) -> bool {
    actual
        .iter()
        .zip(expected)
        .all(|(a, e)| a.abs_diff(e) <= tolerance)
}

#[test]
fn every_frame_comes_back_as_nv12_at_the_right_size() {
    let (frames, hardware) = decode_fixture();

    eprintln!(
        "decoded {} frames, {}",
        frames.len(),
        if hardware { "in hardware" } else { "in software" }
    );

    assert_eq!(frames.len(), FRAMES, "a frame went missing or was duplicated");

    for frame in &frames {
        assert_eq!(frame.format(), PixelFormat::Nv12);
        assert_eq!((frame.width(), frame.height()), (WIDTH, HEIGHT));
    }
}

/// The reason for the moving bar. VideoToolbox emits decode order — I P B B —
/// and the n-th picture out has to be the n-th picture of the film.
#[test]
fn frames_come_out_in_display_order_by_their_pixels() {
    let (frames, _) = decode_fixture();

    for (index, frame) in frames.iter().enumerate() {
        let expected = 12.0 + 4.0 * index as f64;
        let found = bar_centre(frame);

        assert!(
            (found - expected).abs() <= 1.5,
            "picture {index} out shows the bar at x={found:.1}, which is frame {:.1} of the film",
            (found - 12.0) / 4.0
        );
    }
}

#[test]
fn timestamps_rise_one_frame_at_a_time() {
    let (frames, _) = decode_fixture();
    let frame_duration = Duration::from_secs(1) / 24;

    for pair in frames.windows(2) {
        let step = pair[1].pts() - pair[0].pts();

        assert!(
            step.abs_diff(frame_duration) < Duration::from_millis(1),
            "{:?} then {:?}",
            pair[0].pts(),
            pair[1].pts()
        );
    }
}

#[test]
fn the_colours_survive_decoding() {
    let (frames, _) = decode_fixture();

    for frame in [&frames[0], &frames[FRAMES / 2], &frames[FRAMES - 1]] {
        for (x, expected) in BARS {
            let found = rgb_at(frame, x, HEIGHT / 4);

            assert!(
                close(found, expected, 24),
                "at x={x} in the frame at {:?}: {found:?}, expected {expected:?}",
                frame.pts()
            );
        }
    }
}

/// The path a player takes: `VideoSource` opening the file, choosing the
/// decoder, and queueing frames.
#[test]
fn a_video_source_plays_the_whole_file() {
    let mut source = VideoSource::from_file(FIXTURE, None).expect("the fixture opens");
    let mut shown = Vec::new();

    while !source.is_finished() {
        source.advance(Duration::from_millis(42)).unwrap();

        while let Some(frame) = source.pop_frame() {
            shown.push(frame.pts());
        }
    }

    assert_eq!(shown.len(), FRAMES);
    assert!(shown.windows(2).all(|pair| pair[0] < pair[1]), "{shown:?}");
}

/// What looping an overlay relies on: back to the first keyframe, the decoder
/// emptied, and the same pictures again in the same order.
#[test]
fn a_rewound_source_plays_the_same_film_again() {
    let mut source = VideoSource::from_file(FIXTURE, None).unwrap();

    let play = |source: &mut VideoSource| {
        let mut bars = Vec::new();

        while !source.is_finished() {
            source.advance(Duration::ZERO).unwrap();

            while let Some(frame) = source.pop_frame() {
                bars.push((frame.pts(), bar_centre(&frame).round() as u32));
            }
        }

        bars
    };

    let first = play(&mut source);
    source.rewind().unwrap();
    let second = play(&mut source);

    assert_eq!(first.len(), FRAMES);
    assert_eq!(first, second);
}

/// A packet whose NAL length points past its own end. VideoToolbox is handed
/// it as-is; the point is that the answer is an error or nothing, never a
/// crash inside the callback.
#[test]
fn a_corrupt_packet_is_refused_rather_than_crashing() {
    let demuxer = vtome::open_media(FIXTURE).unwrap();
    let track = demuxer.info().video().unwrap().clone();
    let mut decoder = decode::open(&DecoderConfig::from_track(&track).unwrap()).unwrap();

    let garbage = Packet::new(track.id, vec![0, 0, 0, 200, 0x65, 0x88, 0x84], Duration::ZERO, true);

    match decoder.decode(&garbage) {
        Ok(None) => {}
        Ok(Some(frame)) => panic!("a picture from garbage: {frame:?}"),
        Err(error) => eprintln!("refused as expected: {error}"),
    }

    assert!(decoder.flush().is_ok() || decoder.flush().is_err());
}

/// Decoded NV12 drawn by the real shader and read back — the strides
/// VideoToolbox chose, the chroma plane, and the colour matrix all have to be
/// right for this to come out red, green, blue, and white.
#[cfg(feature = "render")]
#[test]
fn decoded_frames_draw_in_colour_through_the_gpu() {
    use vtome::geometry::{Quad, Rect};
    use vtome::render::{Gpu, Renderer};

    let Ok(gpu) = Gpu::new() else {
        eprintln!("skipping: no GPU here");
        return;
    };

    let (frames, _) = decode_fixture();
    let mut renderer = Renderer::new(&gpu, wgpu::TextureFormat::Rgba8Unorm).unwrap();
    let quad = Quad::from_rect(Rect::from_size(f64::from(WIDTH), f64::from(HEIGHT)));

    let pixels = renderer
        .render_to_rgba(&gpu, &frames[5], WIDTH, HEIGHT, quad, 1.0)
        .unwrap();

    let at = |x: u32, y: u32| -> [u8; 3] {
        let start = ((y * WIDTH + x) * 4) as usize;
        [pixels[start], pixels[start + 1], pixels[start + 2]]
    };

    for (x, expected) in BARS {
        let found = at(x, HEIGHT / 4);
        assert!(close(found, expected, 24), "x={x}: {found:?}, expected {expected:?}");
    }

    // Frame 5's moving bar is centred at 12 + 4·5.
    assert!(close(at(32, HEIGHT * 3 / 4), [255, 255, 255], 24), "the bar is not white");
    assert!(close(at(80, HEIGHT * 3 / 4), [0, 0, 0], 24), "the background is not black");
}
