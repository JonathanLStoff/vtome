# Changelog

Finished work is moved out of `planning/TODO.md` and listed under **Done** below
as it lands (see `.claude/CLAUDE.md` in the sibling crates for the habit).

**Done**

- **The crate exists.** `vtome` is a library for putting an image or a video on
  a specific monitor — or a specific quadrilateral of one — with no FFmpeg
  anywhere in the dependency tree and no codec anyone charges for. 127 tests
- **Corner pinning, perspective-correct.** `geometry::Quad::homography` computes
  the projective map from the unit square onto four arbitrary convex corners
  (Heckbert's closed form — no solve, no iteration), and the shader maps each
  *pixel* back through its inverse. Covering the target with one triangle rather
  than pinning two means the divide by `w` happens per pixel: no crease down the
  diagonal, because there is no diagonal. A GPU test asserts the picture's
  midline lands within three pixels of where the maths says it should, on three
  rows of a keystone; `make corner-pin` prints how far wrong the naive version
  is (77 px on a 1920×1080 keystone)
- **A quad that folds over is refused** with `Error::Placement`, at
  configuration time, before a window is opened
- **Placement resolved late.** A `Placement` names a monitor by index, name,
  primary, or the point it contains, and is resolved against the monitors
  actually attached at the moment it is applied — falling back to the primary
  and *reporting that it did*, or refusing if the caller marked the monitor
  required. Physical pixels throughout, because logical pixels across a
  mixed-DPI desktop are a bug generator
- **Colour carried, not guessed away.** `ColorSpace` holds primaries, transfer,
  matrix, and range; `yuv_to_rgb` returns the 3×4 matrix the shader applies, with
  the chroma neutral point computed exactly from the bit depth rather than
  rounded to 0.5. Where a container says nothing, the guess follows resolution
  the way every player's does — and anything the container *does* state wins
- **Frames stay in YUV** all the way to the GPU: I420, I422, I444, NV12, P010,
  and packed RGBA/BGRA, with decoder strides honoured rather than repacked.
  Every layout is validated against its buffer, so a stride that walks off the
  end is an error rather than a read past it. `FramePool` recycles buffers and
  refuses to reclaim one anything else still holds
- **Identification by content.** ISOBMFF split by brand (so AVIF and HEIF are
  told apart from film), EBML by DocType, RIFF by form type (so an AVI is not a
  WebP), plus the still formats. The extension is never consulted
- **Two demuxers, both pure Rust.** MP4/MOV with a lazily-built, remembered
  keyframe index and read-ahead interleaving by decode time; Matroska/WebM
  carrying the colour metadata it actually states. Audio tracks are reported and
  never touched — that is `atome`'s half
- **`bitstream`**: Annex-B ↔ length-prefixed both ways, three- and four-byte
  start codes, and an `avcC` parser that refuses a truncated or over-claiming
  record. The silent-failure spot every H.264 pipeline has
- **`render`**: a wgpu renderer that is not a window. It draws into any texture
  view — a window's, an embedder's, or an offscreen one — and `render_to_rgba`
  reads pixels back for thumbnails, exports, and tests
- **`window`**: a `Viewer` that opens an undecorated, transparent window on the
  monitor you named. `make show FILE=x.png MONITOR=1 KEYSTONE=200`
- **`clock`**: play/pause/seek/rate that survives being paused twice, a
  `MasterClock` trait so video can chase an audio clock, and pacing that presents,
  waits, or drops — with counters, because a stuttering player has to be
  diagnosable
- **Everything heavy is optional.** The default build compiles no GPU
  abstraction, no windowing library, and no C toolchain; `render` gives a GPU
  without a window, and `embed` is the Tauri path
- **No decoders yet, said out loud.** `decode` is the trait, the backend
  selection, and an error naming the backend that would have taken the work and
  distinguishing "you did not compile it" from "this machine does not have it".
  Deliberately not a decoder that returns no frames and a black window
