//! What is this file, really?
//!
//! ```sh
//! make identify FILE=clip.mp4
//! cargo run --example identify -- clip.mp4 photo.png
//! ```
//!
//! Reads the first few kilobytes and nothing else, so it answers as fast on a
//! 40 GB master as on a thumbnail. With the `demux` feature — on by default —
//! it goes on to open the container and list the tracks.

use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    let paths: Vec<String> = std::env::args().skip(1).collect();

    if paths.is_empty() {
        eprintln!("usage: identify <file>...");
        eprintln!("       cargo run --example identify -- clip.mp4");
        return Ok(());
    }

    for path in &paths {
        println!("{path}");

        let container = match vtome::identify_path(path) {
            Ok(container) => container,
            Err(error) => {
                println!("  {error}\n");
                continue;
            }
        };

        println!(
            "  container   {container}{}",
            if container.is_image() { " (still)" } else { "" }
        );

        if container.is_image() {
            println!("  a still: `vtome::load_image`, or `make show FILE={path}`\n");
            continue;
        }

        if !container.is_demuxable() {
            println!(
                "  identified, but vtome has no demuxer for {container} — \
                      transcode it to WebM or MP4 first\n"
            );
            continue;
        }

        describe_tracks(path)?;
        println!();
    }

    Ok(())
}

#[cfg(feature = "demux")]
fn describe_tracks(path: &str) -> Result<(), Box<dyn Error>> {
    let media = match vtome::open_media(path) {
        Ok(media) => media,
        Err(error) => {
            println!("  {error}");
            return Ok(());
        }
    };

    let info = media.info();
    println!("  duration    {:.2}s", info.duration.as_secs_f64());
    println!("  seekable    {}", info.seekable);

    for track in &info.tracks {
        let encoding = track
            .encoding
            .map(|encoding| encoding.to_string())
            .unwrap_or_else(|| format!("{} (unrecognised)", track.codec_id));

        print!("  track {:<5} {:?} {encoding}", track.id, track.kind);

        if track.kind == vtome::TrackKind::Video {
            let (width, height) = track.display_size();
            print!("  {width}×{height}");

            if let Some(rate) = track.frame_rate {
                print!("  {}", rate.describe());
            }

            // The point of the whole crate, said out loud.
            if let Some(encoding) = track.encoding {
                print!(
                    "  [{}]",
                    if encoding.is_royalty_free() {
                        "royalty-free"
                    } else {
                        "licensed — decode through the OS, transcode to keep"
                    }
                );
            }
        }

        println!();
    }

    if info.has_audio() {
        println!("  there is audio here; vtome leaves it alone — hand the file to atome");
    }

    Ok(())
}

#[cfg(not(feature = "demux"))]
fn describe_tracks(_path: &str) -> Result<(), Box<dyn Error>> {
    println!("  (built without the `demux` feature, so the tracks are not read)");
    Ok(())
}
