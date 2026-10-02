//! The engine, standalone: an output on a monitor, and clips added and stopped
//! from another thread while it runs.
//!
//! ```sh
//! make engine FILE=clip.mp4
//! cargo run --release --example engine -- clip.mp4 [snapshot-dir]
//! ```
//!
//! Opens a 640×360 output 40 px in from the top-left of the primary monitor,
//! over a translucent dark background, and then, from a second thread:
//!
//! - plays the video
//! - after half a second, lays a caption bar over its bottom edge for two
//!   seconds (`Hold::Time`), and flashes a marker in the corner for 15 frames
//!   (`Hold::Frames` — half a second at the engine's 30)
//! - at one second, saves what the output shows (if a directory was given)
//! - at four seconds, stops the video, and at five, closes
//!
//! Everything that happens is printed, including clips that end on their own
//! — which is where the holds can be seen keeping time.

use std::collections::HashMap;
use std::error::Error;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use vtome::geometry::Rect;
use vtome::{Clip, ColorSpace, Controls, Frame, Hold, PixelFormat, Vtome};

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);

    let Some(video) = args.next().map(PathBuf::from) else {
        eprintln!("usage: engine <video> [snapshot-dir]");
        std::process::exit(2);
    };
    let snapshots = args.next().map(PathBuf::from);

    let mut vtome = Vtome::new(Vec::<String>::new(), 30.0);
    let controls = vtome.controls();

    let script = thread::spawn(move || {
        if let Err(error) = run(&controls, &video, snapshots) {
            eprintln!("script: {error}");
        }

        controls.close();
    });

    // The primary monitor, chosen once the event loop can see it.
    vtome.start_with(vtome::Audio::Off, |monitors| {
        let primary = monitors
            .iter()
            .find(|monitor| monitor.is_primary)
            .or_else(|| monitors.first());

        let Some(primary) = primary else {
            return (HashMap::new(), HashMap::new());
        };

        println!(
            "output on {} ({}), {}×{} at ({}, {})",
            primary.name, primary.persistent_id, 640, 360, 40, 40
        );

        (
            HashMap::from([(primary.persistent_id.clone(), (640, 360, 40, 40))]),
            HashMap::from([(primary.persistent_id.clone(), "#101828E0".to_string())]),
        )
    })?;

    script.join().ok();
    println!("closed");

    Ok(())
}

/// What a control application would do, from its own thread.
fn run(controls: &Controls, video: &PathBuf, snapshots: Option<PathBuf>) -> Result<(), Box<dyn Error>> {
    // Wait for the output to open; `start_with` is still choosing it.
    let started = Instant::now();
    let monitor = loop {
        if let Some(monitor) = controls.running_monitors().into_iter().next() {
            break monitor;
        }
        if started.elapsed() > Duration::from_secs(5) {
            return Err(format!("no output opened; skipped: {:?}", controls.skipped_monitors()).into());
        }
        thread::sleep(Duration::from_millis(10));
    };

    let clock = Instant::now();
    let at = |seconds: f64| {
        let due = Duration::from_secs_f64(seconds);
        if let Some(wait) = due.checked_sub(clock.elapsed()) {
            thread::sleep(wait);
        }
    };

    let film = controls.add(Clip::video(video, &monitor))?;
    println!("{:>5.2}s  added {film}: {}", clock.elapsed().as_secs_f64(), video.display());

    at(0.5);
    let caption = controls.add(
        Clip::frame(caption_bar(640, 60), &monitor)
            .area(Rect::new(0.0, 300.0, 640.0, 60.0))
            .hold(Hold::Time(Duration::from_secs(2))),
    )?;
    println!("{:>5.2}s  added {caption}: a caption bar for 2 s", clock.elapsed().as_secs_f64());

    let flash = controls.add(
        Clip::frame(caption_bar(40, 40), &monitor)
            .area(Rect::new(590.0, 10.0, 40.0, 40.0))
            .hold(Hold::Frames(15)),
    )?;
    println!("{:>5.2}s  added {flash}: a marker for 15 frames", clock.elapsed().as_secs_f64());

    at(1.0);
    if let Some(directory) = &snapshots {
        std::fs::create_dir_all(directory)?;
        let shot = controls.snapshot_output(&monitor)?;
        let path = directory.join("engine-1s.png");
        image::save_buffer(&path, shot.data(), shot.width(), shot.height(), image::ExtendedColorType::Rgba8)?;
        println!("{:>5.2}s  saved {}", clock.elapsed().as_secs_f64(), path.display());
    }

    // Endings, to the nearest 50 ms.
    while clock.elapsed() < Duration::from_secs(4) {
        report_ended(controls, &clock);
        thread::sleep(Duration::from_millis(50));
    }

    println!(
        "{:>5.2}s  stop {film}: {}",
        clock.elapsed().as_secs_f64(),
        if controls.stop(film) { "stopped" } else { "had already ended" }
    );
    println!(
        "{:>5.2}s  stop {caption}: {}",
        clock.elapsed().as_secs_f64(),
        if controls.stop(caption) { "stopped" } else { "had already ended" }
    );

    at(5.0);
    report_ended(controls, &clock);

    println!(
        "{:>5.2}s  {} frames drawn — {:.1} a second",
        clock.elapsed().as_secs_f64(),
        controls.frames(),
        controls.frames() as f64 / clock.elapsed().as_secs_f64()
    );

    Ok(())
}

fn report_ended(controls: &Controls, clock: &Instant) {
    for ended in controls.take_ended() {
        println!(
            "{:>5.2}s  {} ended{}",
            clock.elapsed().as_secs_f64(),
            ended.id,
            ended.error.map(|error| format!(": {error}")).unwrap_or_default()
        );
    }
}

/// A half-transparent white bar with a solid stripe down its left edge — a
/// stand-in for a lower-third graphic.
fn caption_bar(width: u32, height: u32) -> Frame {
    let mut pixels = Vec::with_capacity((width * height * 4) as usize);

    for _ in 0..height {
        for x in 0..width {
            pixels.extend_from_slice(if x < 12 {
                &[255, 180, 0, 255]
            } else {
                &[255, 255, 255, 170]
            });
        }
    }

    Frame::packed(
        width,
        height,
        PixelFormat::Rgba8,
        ColorSpace::srgb(),
        Duration::ZERO,
        pixels,
    )
    .expect("the buffer is the size of the picture")
}
