//! Put a picture on a monitor — square, or corner-pinned into a trapezoid.
//!
//! ```sh
//! make show FILE=poster.png              # primary monitor, letterboxed
//! make show FILE=poster.png MONITOR=1    # the second monitor
//! make show FILE=poster.png KEYSTONE=0.15 # trapezoid: top edge 15% narrower
//! make monitors                          # what is attached
//! ```
//!
//! Escape or close the window to quit. The window is undecorated and
//! transparent, so a keystoned picture shows the desktop around it rather than
//! a black box.

use std::env;
use std::error::Error;

use vtome::window::{monitors, Viewer};
use vtome::{Fit, MonitorSelector, Placement};

fn main() -> Result<(), Box<dyn Error>> {
    // `make monitors` — list and stop, since an event loop is once per process.
    if env::var("LIST").is_ok() {
        for (index, monitor) in monitors()?.iter().enumerate() {
            println!(
                "{index}: {:<28} {:>5}×{:<5} at ({:>6}, {:>5})  scale {:.2}{}{}",
                if monitor.name.is_empty() {
                    "(unnamed)"
                } else {
                    &monitor.name
                },
                monitor.bounds.width,
                monitor.bounds.height,
                monitor.bounds.x,
                monitor.bounds.y,
                monitor.scale_factor,
                monitor
                    .refresh_hz()
                    .map(|hz| format!("  {hz:.2} Hz"))
                    .unwrap_or_default(),
                if monitor.is_primary {
                    "  [primary]"
                } else {
                    ""
                },
            );
        }

        return Ok(());
    }

    let Some(path) = env::args().nth(1).or_else(|| env::var("FILE").ok()) else {
        eprintln!("usage: show <image>");
        eprintln!("       make show FILE=poster.png MONITOR=1 KEYSTONE=0.15");
        return Ok(());
    };

    let frame = vtome::load_image(&path)?;
    println!("{}: {}×{}", path, frame.width(), frame.height());

    let selector = match env::var("MONITOR") {
        Ok(value) => match value.parse::<usize>() {
            Ok(index) => MonitorSelector::Index(index),
            Err(_) => MonitorSelector::Name(value),
        },
        Err(_) => MonitorSelector::Primary,
    };

    let mut placement = Placement::new(selector).fit(Fit::Contain);

    // The trapezoid, as a fraction of the monitor's width rather than pixels of
    // it: asking how big the monitor is would mean a second event loop, and
    // there is only one of those per process.
    if let Ok(inset) = env::var("KEYSTONE") {
        let inset: f64 = inset.parse()?;
        placement = placement.keystone(inset);

        println!(
            "corner-pinned: top edge inset {:.0}% of the width at each end",
            inset * 100.0
        );
    }

    if let Ok(opacity) = env::var("OPACITY") {
        placement = placement.opacity(opacity.parse()?);
    }

    println!("Escape to quit.");

    Viewer::new(frame, placement)
        .title(format!("vtome — {path}"))
        .show()?;

    Ok(())
}
