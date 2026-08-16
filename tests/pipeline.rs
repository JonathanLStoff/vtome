//! The crate from the outside: identify, place, and draw, through the public
//! API only.
//!
//! The unit tests check the pieces. These check that the pieces meet — which is
//! where a crate with a clean interior and a wrong seam still fails.

use std::time::Duration;

use vtome::color::ColorSpace;
use vtome::frame::{Frame, PixelFormat};
use vtome::geometry::{Point, Quad, Rect};
use vtome::{Container, Fit, MonitorSelector, Placement};

/// A monitor layout that is deliberately awkward: a scaled laptop display with
/// a 4K screen to its right and a projector to its left, at negative
/// coordinates.
fn desktop() -> Vec<vtome::Monitor> {
    vec![
        vtome::Monitor {
            is_primary: true,
            scale_factor: 2.0,
            ..vtome::Monitor::new("Built-in", Rect::new(0.0, 0.0, 3024.0, 1964.0))
        },
        vtome::Monitor {
            refresh_millihertz: Some(59_940),
            ..vtome::Monitor::new("DELL U2720Q", Rect::new(3024.0, 0.0, 3840.0, 2160.0))
        },
        vtome::Monitor::new("EPSON", Rect::new(-1920.0, 0.0, 1920.0, 1080.0)),
    ]
}

fn picture(width: u32, height: u32) -> Frame {
    Frame::packed(
        width,
        height,
        PixelFormat::Rgba8,
        ColorSpace::srgb(),
        Duration::ZERO,
        vec![255; (width * height * 4) as usize],
    )
    .unwrap()
}

/// A file is what its bytes say, whatever it is called — the whole reason
/// identification does not look at the extension.
#[test]
fn a_mislabelled_file_is_identified_by_content() {
    let temp = tempfile::TempDir::new().unwrap();
    let path = temp.path().join("definitely-a-video.mp4");

    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    png.extend_from_slice(&[0; 64]);
    std::fs::write(&path, png).unwrap();

    assert_eq!(vtome::identify_path(&path).unwrap(), Container::Png);
}

/// A still image goes through the same placement machinery as a film, and comes
/// out as desktop coordinates on the monitor that was asked for.
#[test]
fn a_picture_lands_on_the_monitor_it_was_sent_to() {
    let monitors = desktop();
    let frame = picture(1920, 1080);

    let placement = Placement::new(MonitorSelector::Name("EPSON".to_string())).fit(Fit::Contain);

    let resolved = placement
        .resolve(frame.width(), frame.height(), &monitors)
        .unwrap();

    assert_eq!(resolved.monitor.name, "EPSON");
    assert!(!resolved.fell_back);

    // The projector is to the left of the primary display, so its coordinates
    // are negative — the case that breaks anything assuming a desktop starts at
    // the origin.
    let rect = resolved.window_rect();
    assert_eq!(rect.x, -1920.0);
    assert_eq!((rect.width, rect.height), (1920.0, 1080.0));

    // In the window's own space it starts at zero again, which is what the
    // renderer needs.
    let local = resolved.quad_in_window();
    assert_eq!(local.corners[0], Point::new(0.0, 0.0));
}

/// The show that loses its projector between rehearsal and performance.
#[test]
fn a_missing_monitor_falls_back_and_says_so() {
    let monitors = vec![desktop()[0].clone()];

    let placement = Placement::new(MonitorSelector::Name("EPSON".to_string()));
    let resolved = placement.resolve(1920, 1080, &monitors).unwrap();

    assert!(resolved.fell_back);
    assert_eq!(resolved.monitor.name, "Built-in");

    // ...unless the caller says the monitor is not optional.
    let strict = Placement::new(MonitorSelector::Name("EPSON".to_string())).require_monitor(true);

    assert!(matches!(
        strict.resolve(1920, 1080, &monitors),
        Err(vtome::Error::NoSuchMonitor { .. })
    ));
}

/// Corner pinning survives the trip from configuration to shader coordinates:
/// the quad is written in monitor space, resolved into desktop space, and
/// handed to the renderer in window space, and the shape has to be the same
/// shape at every step.
#[test]
fn a_keystone_keeps_its_shape_through_every_coordinate_change() {
    let monitors = desktop();
    let quad = Quad::keystone(Rect::from_size(1920.0, 1080.0), 240.0);

    let placement = Placement::new(MonitorSelector::Index(2)).corners(quad);
    let resolved = placement.resolve(1920, 1080, &monitors).unwrap();

    let local = resolved.quad_in_window();

    // The projective map exists at both ends...
    assert!(quad.homography().is_ok());
    assert!(local.homography().is_ok());

    // ...and the shape is unchanged: same edge lengths, still not affine.
    for index in 0..4 {
        let original = quad.corners[index].distance(quad.corners[(index + 1) % 4]);
        let moved = local.corners[index].distance(local.corners[(index + 1) % 4]);

        assert!(
            (original - moved).abs() < 1e-9,
            "edge {index} changed length"
        );
    }

    assert!(!local.is_affine());
}

/// Nothing about a keystone is drawable if the corners fold over, and that has
/// to be caught while it is still configuration rather than at draw time.
#[test]
fn a_folded_quad_never_reaches_the_renderer() {
    let monitors = desktop();

    let bowtie = Quad::new([
        Point::new(0.0, 0.0),
        Point::new(100.0, 0.0),
        Point::new(0.0, 100.0),
        Point::new(100.0, 100.0),
    ]);

    let placement = Placement::new(MonitorSelector::Primary).corners(bowtie);

    assert!(matches!(
        placement.resolve(64, 64, &monitors),
        Err(vtome::Error::Placement { .. })
    ));
}

/// The pairing with an audio engine: video chases whatever clock it is given,
/// and drops rather than falls behind.
#[test]
fn playback_follows_an_external_clock() {
    use vtome::clock::Action;
    // The trait has to be in scope to call `position` through it.
    use vtome::MasterClock;

    struct AudioClock(Duration);

    impl vtome::MasterClock for AudioClock {
        fn position(&self) -> Duration {
            self.0
        }
    }

    let master = AudioClock(Duration::from_millis(1_000));
    let mut pacing = vtome::Pacing::for_frame_rate(24.0);

    assert_eq!(
        pacing.decide(Duration::from_millis(1_000), master.position()),
        Action::Present
    );
    assert_eq!(
        pacing.decide(Duration::from_millis(200), master.position()),
        Action::Drop,
        "a frame 800 ms late is not worth drawing"
    );
    assert!(matches!(
        pacing.decide(Duration::from_millis(2_000), master.position()),
        Action::Wait(_)
    ));

    let (presented, dropped, _) = pacing.counts();
    assert_eq!((presented, dropped), (1, 1));
}

/// Only the royalty-free encodings can be written. This is the crate's reason
/// for existing, asserted from outside so that it cannot quietly change.
#[test]
fn nothing_patent_encumbered_is_ever_encodable() {
    use vtome::Encoding;

    for encoding in [Encoding::H264, Encoding::H265, Encoding::ProRes] {
        assert!(!encoding.is_encodable(), "{encoding} must not be written");
    }

    for encoding in [Encoding::Av1, Encoding::Vp9] {
        assert!(encoding.is_encodable());
        assert!(encoding.is_royalty_free());
    }
}

/// Decoding is honest about being unimplemented: an error naming what is
/// missing, rather than a decoder that returns nothing.
#[test]
fn asking_for_a_decoder_fails_with_an_explanation() {
    use vtome::decode::{self, DecoderConfig};

    let config = DecoderConfig {
        encoding: vtome::Encoding::Av1,
        width: 1920,
        height: 1080,
        bit_depth: 8,
        color: ColorSpace::default(),
        extra_data: Vec::new(),
    };

    let Err(vtome::Error::NoDecoder { remedy, .. }) = decode::open(&config) else {
        panic!("no decoder backend is implemented yet");
    };

    assert!(
        !remedy.is_empty(),
        "an unhelpful refusal is worse than none"
    );
}

/// The still-image path, end to end: load, place, and know how big it will be
/// on screen.
#[cfg(feature = "image")]
#[test]
fn an_image_file_becomes_a_frame_and_a_placement() {
    let temp = tempfile::TempDir::new().unwrap();
    let path = temp.path().join("poster.png");

    image::RgbaImage::from_pixel(320, 180, image::Rgba([10, 20, 30, 255]))
        .save(&path)
        .unwrap();

    let frame = vtome::load_image(&path).unwrap();
    assert_eq!((frame.width(), frame.height()), (320, 180));

    let placement = Placement::new(MonitorSelector::Index(1)).fit(Fit::Contain);
    let resolved = placement
        .resolve(frame.width(), frame.height(), &desktop())
        .unwrap();

    // 16:9 into a 16:9 monitor: no bars, edge to edge.
    let rect = resolved.window_rect();
    assert_eq!((rect.width, rect.height), (3840.0, 2160.0));
}

/// A file that is not a container says so, rather than failing somewhere deep
/// in a parser.
#[cfg(feature = "demux")]
#[test]
fn opening_something_that_is_not_media_fails_early() {
    let temp = tempfile::TempDir::new().unwrap();
    let path = temp.path().join("notes.txt");
    std::fs::write(&path, b"there is no video here").unwrap();

    assert!(matches!(
        vtome::open_media(&path),
        Err(vtome::Error::UnknownContainer { .. })
    ));
}
