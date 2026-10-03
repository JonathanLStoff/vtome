//! Show an image or a video on a specific monitor — or on a specific
//! quadrilateral of one — without FFmpeg and without a codec anyone charges
//! for.
//!
//! ```text
//! file.mp4 ──demux──▶ H.264 ──platform decode──▶ ┌─────────┐ ──▶ GPU ──▶ corner-pinned quad
//!                                                │  Frame  │            on monitor 2
//! file.webm ─demux──▶ AV1 ──────software────────▶ │ (YUV +  │
//!                                                │  colour)│ ──▶ H.264 (OS) or AV1 out (import)
//! image.png ──────────────image─────────────────▶└─────────┘
//! ```
//!
//! # What this is for
//!
//! Putting a picture where you want it: on the second monitor, in a rectangle
//! of the third, or into four arbitrary corners because the projector is not
//! square to the wall. [`geometry::Quad`] is that last one, and it is
//! perspective-correct — see its documentation for why the obvious
//! implementation leaves a crease down the diagonal.
//!
//! Audio is deliberately absent. `atome` is the audio engine; every clip here
//! follows one engine timeline, which with [`Audio::atome`](crate::Audio) *is*
//! atome's output clock (see [`clock`]) — vtome never opens a device.
//!
//! # Nothing patent-pooled is ever bundled
//!
//! vtome reads H.264 and AV1, and writes them: H.264 by default, but only
//! ever through the operating system's own codec — VideoToolbox today — whose
//! vendor already holds the licence; AV1 through the bundled, royalty-free
//! `rav1e`, wherever the OS has no H.264 encoder or when asked. HEVC and VP9
//! are out of scope. [`import`](crate::import()) writes a proxy at the output
//! path first and the full-quality file through a queue — see
//! `planning/TODO.md` §14 and §15.
//!
//! # Everything heavy is optional
//!
//! A build that only decodes frames and hands them to somebody else's renderer
//! must not compile a windowing library, a GPU abstraction, or a C toolchain.
//!
//! | Feature | What it adds |
//! |---|---|
//! | `demux` | MP4 and Matroska/WebM parsing (pure Rust) |
//! | `image` | Still images |
//! | `render` | GPU presentation via wgpu — a surface, not a window |
//! | `window` | `render` plus winit, so vtome opens its own windows |
//! | `embed` | `render` against a surface someone else owns — the Tauri path |
//! | `decode-*` | One decoder backend each |
//! | `encode-platform`, `encode-av1`, `mux` | H.264 through the OS into MP4, AV1 through rav1e into WebM |
//! | `transcode` (= `import`) | [`transcode`] and the [`import`](crate::import()) queue; rav1e always comes with it |
//! | `atome`, `split-audio` | Video following atome's clock; a film's audio to FLAC on import |
//!
//! # A first look
//!
//! ```no_run
//! use vtome::{identify, Placement, MonitorSelector};
//!
//! // What is this file, really? The extension is a claim; the bytes are not.
//! let container = identify::identify_path("clip.mp4")?;
//! println!("{container}");
//!
//! // Second monitor, top-left quarter of it.
//! let placement = Placement::new(MonitorSelector::Index(1))
//!     .area(vtome::geometry::Rect::from_size(960.0, 540.0));
//! # Ok::<(), vtome::Error>(())
//! ```

#![warn(missing_docs)]

pub mod bitstream;
pub mod clock;
pub mod color;
pub mod decode;
mod error;
pub mod frame;
pub mod geometry;
pub mod identify;
pub mod media;
pub mod output;
pub mod output_layer;
pub mod placement;
pub mod plugin;
pub mod scale;

// Writing: H.264 through the OS encoder, AV1 through the bundled rav1e.
pub mod encode;

#[cfg(all(feature = "encode-platform", target_vendor = "apple"))]
pub mod encode_videotoolbox;

#[cfg(feature = "encode-av1")]
pub mod encode_av1;

// Windows' own H.264 encoder and decoder.
#[cfg(all(windows, any(feature = "decode-platform", feature = "encode-platform")))]
pub mod media_foundation;

// Android's own H.264 encoder and decoder.
#[cfg(all(target_os = "android", any(feature = "decode-platform", feature = "encode-platform")))]
pub mod media_codec;

#[cfg(feature = "demux")]
mod playback;

#[cfg(feature = "demux")]
pub mod demux;

// Containers out: H.264 into MP4, AV1 into WebM.
#[cfg(feature = "mux")]
pub mod mux;

// One file through decode, scale, encode, and mux.
#[cfg(feature = "transcode")]
pub mod transcode;

// The import queue: a proxy at the output path now, the real file later.
#[cfg(feature = "transcode")]
pub mod import;

// A source is a file opened through a demuxer, and the compositor and player
// are built from sources, so all three need one.
#[cfg(feature = "demux")]
pub mod compositor;
#[cfg(feature = "demux")]
pub mod player;
#[cfg(feature = "demux")]
pub mod video_source;

#[cfg(feature = "image")]
pub mod still;

#[cfg(feature = "render")]
pub mod render;

#[cfg(feature = "window")]
pub mod window;

// A clip's soundtrack through atome, on the clock its pictures follow.
#[cfg(all(feature = "atome", feature = "render", feature = "demux", feature = "image", any(feature = "window", feature = "tauri")))]
mod audio;

// The engine an application drives: outputs per monitor, clips added and
// stopped from any thread. Two hosts share it — vtome's own windows, and a
// Tauri application's.
#[cfg(all(
    feature = "render",
    feature = "demux",
    feature = "image",
    any(feature = "window", feature = "tauri")
))]
mod engine;

#[cfg(all(feature = "window", feature = "demux", feature = "image"))]
mod standalone;

#[cfg(feature = "tauri")]
mod tauri_host;

#[cfg(all(feature = "decode-platform", target_vendor = "apple"))]
pub mod decode_videotoolbox;

#[cfg(feature = "decode-av1")]
pub mod decode_av1;

pub use clock::{Clock, Follower, MasterClock, Monotonic, Pacing, SharedClock};
pub use color::ColorSpace;
#[cfg(feature = "demux")]
pub use compositor::{Compositor, CompositorStats};
pub use decode::{Decoder, Hardware};
pub use encode::Encoder;
pub use error::{Error, Result};
pub use frame::{Frame, FramePool, PixelFormat};
pub use geometry::{Fit, Point, Quad, Rect};
pub use identify::{identify_bytes, identify_path, Container, Encoding};
pub use media::{MediaInfo, Packet, TrackInfo, TrackKind};
pub use output::Output;
pub use output_layer::OutputLayer;
pub use placement::{Monitor, MonitorSelector, Placement, ResolvedPlacement};
pub use plugin::Plugin;
#[cfg(feature = "demux")]
pub use player::Player;
#[cfg(feature = "demux")]
pub use video_source::{cache_frames, FrameCache, VideoSource};

#[cfg(feature = "demux")]
pub use demux::{open as open_media, Demuxer};

#[cfg(feature = "image")]
pub use still::load_image;

#[cfg(feature = "transcode")]
pub use import::{
    import, import_progress, import_queue, import_with, ImportOptions, ImportQueue, JobId,
    JobProgress, QueueConfig, Stage,
};

#[cfg(all(
    feature = "render",
    feature = "demux",
    feature = "image",
    any(feature = "window", feature = "tauri")
))]
pub use engine::{Audio, Clip, ClipEnded, ClipId, Controls, Hold, OutputRect};

#[cfg(all(feature = "window", feature = "demux", feature = "image"))]
pub use standalone::Vtome;

#[cfg(feature = "tauri")]
pub use tauri_host::{Started, TauriVtome};
