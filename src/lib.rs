//! Show an image or a video on a specific monitor — or on a specific
//! quadrilateral of one — without FFmpeg and without a codec anyone charges
//! for.
//!
//! ```text
//! file.mp4 ──demux──▶ H.264 ──platform decode──▶ ┌─────────┐ ──▶ GPU ──▶ corner-pinned quad
//!                                                │  Frame  │            on monitor 2
//! file.webm ─demux──▶ AV1 ──────software────────▶ │ (YUV +  │
//!                                                │  colour)│ ──▶ AV1 out (transcode)
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
//! Audio is deliberately absent. `atome` is the audio engine; a player here
//! slaves its clock to one (see [`clock`]) rather than opening a device.
//!
//! # Royalty-free, which is a design constraint rather than a preference
//!
//! vtome *writes* AV1 and VP9, both AOMedia/Google royalty-free. It never
//! writes H.264 or HEVC. It *reads* them through the decoder the operating
//! system already ships and already licensed — VideoToolbox, Media Foundation,
//! MediaCodec, VA-API — which is also the fastest path, since those are the
//! hardware decoders. [`identify::Encoding::is_royalty_free`] is the same rule
//! in code.
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
//! | `encode-av1`, `mux`, `transcode` | Writing AV1 into WebM |
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
pub mod placement;

#[cfg(feature = "demux")]
pub mod demux;

#[cfg(feature = "image")]
pub mod still;

#[cfg(feature = "render")]
pub mod render;

#[cfg(feature = "window")]
pub mod window;

pub use clock::{Clock, MasterClock, Pacing};
pub use color::ColorSpace;
pub use decode::Decoder;
pub use error::{Error, Result};
pub use frame::{Frame, FramePool, PixelFormat};
pub use geometry::{Fit, Point, Quad, Rect};
pub use identify::{identify_bytes, identify_path, Container, Encoding};
pub use media::{MediaInfo, Packet, TrackInfo, TrackKind};
pub use placement::{Monitor, MonitorSelector, Placement, ResolvedPlacement};

#[cfg(feature = "demux")]
pub use demux::{open as open_media, Demuxer};

#[cfg(feature = "image")]
pub use still::load_image;
