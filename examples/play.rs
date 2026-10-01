//! Play a video as an overlay: no window frame, above everything else, on the
//! monitor — or the part of it, or the corner-pinned quad of it — you choose.
//!
//! ```sh
//! make play FILE=clip.mp4                      # primary monitor, letterboxed
//! make play FILE=clip.mp4 MONITOR=1            # the second monitor
//! make play FILE=clip.mp4 AREA=40,40,640,360   # a rectangle of it, in its own pixels
//! make play FILE=clip.mp4 KEYSTONE=0.15        # corner-pinned into a trapezoid
//! make play FILE=clip.mp4 OPACITY=0.8 CLICK_THROUGH=1 ONCE=1
//! ```
//!
//! Escape closes it and Space pauses, while it has focus. With `CLICK_THROUGH`
//! the mouse falls through to whatever is underneath, so nothing can give it
//! focus: pair it with `ONCE` so it ends with the film, or stop it from the
//! terminal. What the decoder did — hardware or not, frames shown and dropped —
//! is printed when it closes.

use std::env;
use std::error::Error;

use vtome::geometry::Rect;
use vtome::window::Viewer;
use vtome::{Fit, MonitorSelector, Placement, VideoSource};

fn main() -> Result<(), Box<dyn Error>> {
    let Some(path) = env::args().nth(1).or_else(|| setting("FILE")) else {
        eprintln!("usage: play <video>");
        eprintln!("       make play FILE=clip.mp4 MONITOR=1 AREA=40,40,640,360");
        std::process::exit(2);
    };

    // Opening the source is where "nothing decodes this" surfaces — before any
    // overlay exists to be left blank.
    let source = VideoSource::from_file(&path, None)?;

    if let Some(track) = source.info().video() {
        println!(
            "{path}: {}×{} {}, {} decode",
            track.width,
            track.height,
            source.decoder().encoding(),
            if source.decoder().is_hardware() {
                "hardware"
            } else {
                "software"
            }
        );
    }

    let selector = match setting("MONITOR") {
        Some(value) => match value.parse::<usize>() {
            Ok(index) => MonitorSelector::Index(index),
            Err(_) => MonitorSelector::Name(value),
        },
        None => MonitorSelector::Primary,
    };

    let mut placement = Placement::new(selector)
        .fit(Fit::Contain)
        .always_on_top(true);

    if let Some(area) = setting("AREA") {
        let numbers: Vec<f64> = area
            .split(',')
            .map(|part| part.trim().parse())
            .collect::<Result<_, _>>()?;

        let [x, y, width, height] = numbers[..] else {
            return Err(format!("AREA is x,y,width,height; got {area}").into());
        };

        placement = placement.area(Rect::new(x, y, width, height));
    }

    // A fraction of the monitor's width rather than pixels of it, as `show`
    // does: asking how big the monitor is would take a second event loop.
    if let Some(inset) = setting("KEYSTONE") {
        placement = placement.keystone(inset.parse()?);
    }

    if let Some(opacity) = setting("OPACITY") {
        placement = placement.opacity(opacity.parse()?);
    }

    let click_through = setting("CLICK_THROUGH").is_some();
    let once = setting("ONCE").is_some();

    println!(
        "{}{}",
        if click_through {
            "Clicks pass through it."
        } else {
            "Escape to quit, Space to pause."
        },
        if once { " Closes when the film ends." } else { "" }
    );

    let report = Viewer::video(source, placement)
        .title(format!("vtome — {path}"))
        .click_through(click_through)
        .looping(!once)
        .show()?;

    println!(
        "{} frames shown, {} dropped, {} refreshes repeated, {} loop{}, {} decode",
        report.presented,
        report.dropped,
        report.repeated,
        report.loops,
        if report.loops == 1 { "" } else { "s" },
        match report.hardware_decode {
            Some(true) => "hardware",
            Some(false) => "software",
            None => "no",
        }
    );

    Ok(())
}

/// An environment setting, treating empty as unset — `make` passes every
/// variable through, set or not.
fn setting(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.is_empty())
}
