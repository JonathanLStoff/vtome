//! Decodes a whole video and saves a contact sheet of it — no window needed.
//!
//! ```sh
//! make contact-sheet FILE=clip.mp4                 # writes clip.contact.png beside it
//! cargo run --release --example contact_sheet -- clip.mp4 sheet.png
//! ```
//!
//! Every frame goes through the platform decoder and the count and speed are
//! printed; six of them, evenly spaced, are drawn by the same GPU renderer a
//! window uses and read back into one PNG. It is the quickest way to see that
//! decoding works — and to see *what* it produced — on a machine where a window
//! is inconvenient.

use std::error::Error;
use std::path::PathBuf;
use std::time::Instant;

use vtome::decode::{self, DecoderConfig};
use vtome::geometry::{Quad, Rect};
use vtome::render::{Gpu, Renderer};
use vtome::Frame;

/// Tiles across and down.
const COLUMNS: u32 = 3;
const ROWS: u32 = 2;
/// Each tile's width; the height follows the film's shape.
const TILE_WIDTH: u32 = 480;

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);

    let Some(input) = args.next().map(PathBuf::from) else {
        eprintln!("usage: contact_sheet <video file> [output.png]");
        std::process::exit(2);
    };

    let output = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| input.with_extension("contact.png"));

    let mut demuxer = vtome::open_media(&input)?;
    let track = demuxer.info().video().ok_or("no video track")?.clone();
    let mut decoder = decode::open(&DecoderConfig::from_track(&track)?)?;

    let started = Instant::now();
    let mut frames: Vec<Frame> = Vec::new();

    while let Some(packet) = demuxer.next_packet()? {
        if packet.track_id == track.id {
            frames.extend(decoder.decode(&packet)?);
        }
    }

    frames.extend(decoder.flush()?);

    let elapsed = started.elapsed();

    println!(
        "{}: {} frames of {}×{} {} decoded in {:.0} ms ({:.0} frames/s), {}",
        input.display(),
        frames.len(),
        track.width,
        track.height,
        track.encoding.map_or_else(|| track.codec_id.clone(), |encoding| encoding.to_string()),
        elapsed.as_secs_f64() * 1000.0,
        frames.len() as f64 / elapsed.as_secs_f64(),
        if decoder.is_hardware() {
            "in hardware"
        } else {
            "in software"
        }
    );

    if frames.is_empty() {
        return Err("nothing decoded, so there is nothing to draw".into());
    }

    let gpu = Gpu::new()?;
    let mut renderer = Renderer::new(&gpu, wgpu::TextureFormat::Rgba8Unorm)?;

    let tile_height = (f64::from(TILE_WIDTH) * f64::from(track.height) / f64::from(track.width))
        .round()
        .max(1.0) as u32;
    let sheet_width = TILE_WIDTH * COLUMNS;
    let sheet_height = tile_height * ROWS;
    let mut sheet = vec![0_u8; (sheet_width * sheet_height * 4) as usize];

    let tiles = (COLUMNS * ROWS) as usize;
    let quad = Quad::from_rect(Rect::from_size(f64::from(TILE_WIDTH), f64::from(tile_height)));

    for tile in 0..tiles {
        // Evenly spaced, first and last included.
        let index = tile * (frames.len() - 1) / (tiles - 1).max(1);
        let frame = &frames[index];

        let pixels = renderer.render_to_rgba(&gpu, frame, TILE_WIDTH, tile_height, quad, 1.0)?;

        let (column, row) = (tile as u32 % COLUMNS, tile as u32 / COLUMNS);

        for y in 0..tile_height {
            let from = (y * TILE_WIDTH * 4) as usize;
            let to = (((row * tile_height + y) * sheet_width + column * TILE_WIDTH) * 4) as usize;
            let length = (TILE_WIDTH * 4) as usize;

            sheet[to..to + length].copy_from_slice(&pixels[from..from + length]);
        }

        println!(
            "  tile {}: frame {index} at {:.3} s",
            tile + 1,
            frame.pts().as_secs_f64()
        );
    }

    image::save_buffer(
        &output,
        &sheet,
        sheet_width,
        sheet_height,
        image::ExtendedColorType::Rgba8,
    )?;

    println!("wrote {}", output.display());

    Ok(())
}
