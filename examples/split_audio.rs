//! Transcodes a video into vtome's default encoding and splits its soundtrack
//! off into a FLAC file, twice: once to paths, once to writers.
//!
//! ```sh
//! make split-audio FILE=camera.mov                # -> target/example/split/
//! make split-audio FILE=camera.mov OUT=elsewhere
//! ```
//!
//! - **By path** is the plain version: `transcode` and atome's `to_flac` each
//!   take a path and make a file.
//! - **By writer** is `transcode_into` and `to_flac_into`, which write wherever
//!   they are pointed. Here that is a pfac bundle, the way a project file holds
//!   its assets: both outputs go into one `.pfac`, with no loose file written
//!   first and none to move afterwards. Anything `Write + Seek` — and, for the
//!   video, `Read` too — would do in its place: a `File`, a `Cursor`.
//!
//! The video is whatever `Settings::default()` says: H.264 through the
//! operating system's encoder, MP4 with the index first. The audio is lossless
//! FLAC, 16-bit for 8- and 16-bit sources and 24-bit for everything else, and
//! is decoded from the same input file, so the two outputs are made
//! independently and a file with no soundtrack simply gets no FLAC.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use atome::export::{to_flac, to_flac_into, FlacSummary};
use pfac::{BundleOptions, BundleWriter};
use vtome::transcode::{transcode, transcode_into, Settings, Summary};

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args().skip(1);

    let Some(input) = arguments.next().map(PathBuf::from) else {
        eprintln!("usage: split_audio <video> [output directory]");
        eprintln!("       make split-audio FILE=camera.mov");
        return Ok(());
    };

    let out = arguments
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/example/split"));
    fs::create_dir_all(&out)?;

    println!("{}\n", input.display());

    by_path(&input, &out)?;
    println!();
    by_writer(&input, &out)?;

    Ok(())
}

/// Each output is a path; the libraries make the files.
fn by_path(input: &Path, out: &Path) -> Result<(), Box<dyn Error>> {
    println!("by path -> {}", out.display());

    let video = out.join("video.mp4");
    let summary = transcode(
        input,
        &video,
        &Settings::default(),
        progress("  video"),
        &AtomicBool::new(false),
    )?;
    println!("\r{}", describe_video(&summary, &video));

    let audio = out.join("audio.flac");
    match to_flac(input, &audio) {
        Ok(summary) => println!("  audio  {}", describe_audio(&summary, &audio)),
        Err(error) => {
            // A video with no soundtrack is not a failure; there is just nothing
            // to split. The failed attempt may have left an empty file.
            let _ = fs::remove_file(&audio);
            println!("  audio  none ({error})");
        }
    }

    Ok(())
}

/// Each output is a writer, here one entry of a bundle apiece.
fn by_writer(input: &Path, out: &Path) -> Result<(), Box<dyn Error>> {
    let bundle_path = out.join("project.pfac");
    println!("by writer -> {}", bundle_path.display());

    // Stored, so the entries can be streamed back out in place.
    let mut bundle = BundleWriter::create(&bundle_path, &BundleOptions::stored())?;

    // `transcode_into` needs `Read + Write + Seek + Send`: it patches what it
    // wrote, and MP4's index is moved to the front in the destination itself. A
    // bundle entry is all four. Pass `&mut entry` to keep it, to finish.
    let mut entry = bundle.entry("video/clip.mp4")?;
    let summary = transcode_into(
        input,
        &mut entry,
        &Settings::default(),
        progress("  video"),
        &AtomicBool::new(false),
    )?;
    let bytes = entry.finish()?;
    println!(
        "\r  video  video/clip.mp4  {}  ({bytes} bytes in the bundle)",
        describe(&summary)
    );

    // `to_flac_into` needs only `Write + Seek`: the header's sample count and
    // checksum are patched in at the end.
    let mut entry = bundle.entry("audio/clip.flac")?;
    match to_flac_into(input, &mut entry) {
        Ok(summary) => {
            let bytes = entry.finish()?;
            println!(
                "  audio  audio/clip.flac  {}  ({bytes} bytes in the bundle)",
                describe_flac(&summary)
            );
        }
        // Dropped, not finished: nothing of it reaches the bundle.
        Err(error) => println!("  audio  none ({error})"),
    }

    // Until this, there is no bundle on disk at all.
    bundle.finish()?;

    let bundle = pfac::Bundle::open(&bundle_path)?;
    println!("  bundle holds {:?}", bundle.names().collect::<Vec<_>>());

    Ok(())
}

/// A progress callback that rewrites one line.
fn progress(label: &'static str) -> impl FnMut(vtome::transcode::Progress) {
    move |progress| {
        print!("\r{label}  {:>3.0}%", progress.fraction * 100.0);
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }
}

fn describe(summary: &Summary) -> String {
    format!(
        "{} in {}, {}x{} at {:.2} fps, {}-bit, {} frames, {:.1} s{}",
        summary.encoding,
        summary.container,
        summary.width,
        summary.height,
        summary.frame_rate.as_f64(),
        summary.bit_depth,
        summary.frames,
        summary.duration.as_secs_f64(),
        if summary.encoder_hardware { ", hardware encoder" } else { "" },
    )
}

fn describe_video(summary: &Summary, path: &Path) -> String {
    format!("  video  {}  {}", path.display(), describe(summary))
}

fn describe_flac(summary: &FlacSummary) -> String {
    format!(
        "{} Hz, {} ch, {}-bit, {} frames per channel, {:.1} s",
        summary.sample_rate,
        summary.channels,
        summary.bits_per_sample,
        summary.frames,
        summary.duration(),
    )
}

fn describe_audio(summary: &FlacSummary, path: &Path) -> String {
    format!("{}  {}", path.display(), describe_flac(summary))
}
