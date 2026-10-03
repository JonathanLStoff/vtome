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
- **H.264 through VideoToolbox, in hardware** (`decode-platform`, Apple
  targets). Plain `extern "C"` into VideoToolbox, CoreMedia, CoreVideo, and
  CoreFoundation, every declaration checked against the SDK headers — no
  Objective-C runtime and no crates. `avcC` → format description; MP4's
  length-prefixed samples go in untouched; NV12 comes out with VideoToolbox's
  own strides and goes straight to the shader. `is_hardware()` asks the session
  rather than assuming. A 1280×720 file decodes at ~775 frames/s on an M1
- **Display order out of decode order.** VideoToolbox emits decode order; a
  reorder queue releases each picture once nothing still to be decoded can be
  shown before it — exact where the container has decode times (MP4/MOV,
  including negative composition offsets), a four-frame window where it does
  not (Matroska)
- **MP4 timestamps were swapped.** The `mp4` crate's `start_time` is the decode
  time; it was being read as presentation time, so every file with B-frames had
  PTS and DTS the wrong way round. `tests/mp4_timing.rs` fails on the old code
- **`VideoSource` refuses what it cannot decode** instead of quietly swapping in
  a stub decoder that produced no frames, and gained `rewind()`, `info()`, and
  `decoder()`. `decode::open` now tries each candidate backend in turn
- **Films as overlays.** `Viewer::video(source, placement)` plays a file the
  way `Viewer::new` shows a still: undecorated, transparent around the picture,
  optionally always on top and click-through, paced by `Pacing` against a clock
  that starts once the window is actually on screen. `show()` returns a
  `Report` of frames presented, dropped, and repeated. `make play` and
  `make contact-sheet` are the examples. The surface now asks for an alpha mode
  the desktop can see through rather than whichever the platform lists first
- **Decode tests against a real H.264 file.** `tests/data/bars_h264.mp4` (2.7 KB,
  24 frames, B-frames, rebuilt by `make_h264_fixture.sh`) carries a bar that
  moves every frame, so display order is checked from the pixels; colour is
  checked both on the CPU and through the GPU shader; a corrupt packet is an
  error rather than a crash; rewinding plays the same pictures again
- **Codec scope: decode H.264 and AV1, write AV1.** `is_encodable()` is AV1
  only; HEVC and VP9 are out of scope
- `--no-default-features` builds again: the source, compositor, and player
  modules need a demuxer and now say so
- **The compositor draws its layers.** `Renderer::draw_layers` clears once and
  composites any number of `Picture`s in one pass — each with its own textures
  and uniforms, so a second layer no longer erases the first — and the result
  is premultiplied, which is what a see-through window wants.
  `Compositor::draw_output` puts the background colour or image (covering, like
  a wallpaper) under every visible layer, stacked by z-index (0 on top, equal
  z-indices newest on top), scaled from the output's coordinates to the
  target's, re-uploading a picture only when its source changed. Sources are
  films or stills; a still runs until removed or for a number of frames.
  `tick` reports sources that ended or failed rather than stopping the show for
  one bad file. `render_output_to_rgba` reads an output back, in RGBA or BGRA
- **`Vtome`, the engine an application drives, in two versions with one API.**
  `new(excluded_monitors, frame_rate)`; `start(outputs, backgrounds)` with
  OrbitX's `HashMap<String, (w, h, x, y)>` (monitor-relative, physical pixels)
  and `#RRGGBBAA` or an image path per monitor; `add(Clip)` → `ClipId` for a
  video, an image, or a frame, with an area, fit, opacity, z-index, and — for
  stills — a duration counted as `time × frame rate` frames; `stop(id)`. Clips
  open on the caller's thread, so a bad file is an error at `add`, not a gap on
  screen. `Controls` does all of it from any thread and adds `snapshot`,
  `take_ended`, and `skipped_monitors`. Outputs are borderless, always on top,
  click-through, and never take focus
  - **Standalone** (`window`): vtome's own winit loop; `start_with` hands over
    the attached monitors first, since the loop is the only way to list them
  - **Tauri** (`tauri`, for OrbitX): bare, webview-less Tauri windows made on
    Tauri's main thread, with everything else on vtome's own thread. Made
    see-through on macOS with public AppKit rather than Tauri's
    `macos-private-api`. Checked in a scratch Tauri 2 app: the output opens in
    0.65 s, plays, stops, and closes
- **Monitor ids are stable and agree across both versions.** One
  `placement::monitor_id` — name, desktop position, scale factor — hashed with
  FNV-1a written out, because `DefaultHasher` may change between Rust releases
  and would rename every monitor in a saved settings file. No refresh rate,
  which Tauri does not report. winit and Tauri's tao name a Mac display the same
  way, so both versions gave the built-in display `monitor_309c10e50ce2f28c`.
  Pinned by a test
- **`import`: H.264 by default, a proxy now, the real file later, through a
  queue** (planning §14, asked for 2026-10-01). `vtome::import(input, output,
  output_audio)` checks the input at the call — container, video, a decoder,
  an encoder — and returns a `JobId`. A proxy (≤ 640 px, fast settings) is
  written beside the output and renamed onto it, so the path plays at once;
  the full-quality file encodes in a temp directory and replaces it in one
  rename (copy-then-rename across volumes). Two bounded lanes — proxies and
  audio, final encodes — one worker each by default: five files queued never
  ran more than one encoder per lane. `import_progress()` reports every file:
  stage, fractions, frames, codec, size, whether the output plays yet,
  hardware used, error. `cancel` and `wait`. `ImportOptions::hardware` is
  `Prefer`/`Require`/`Off` and reaches decoder and encoder alike
- **§15's spec in every H.264 file**: High profile with Level 4.1 *declared*,
  CABAC, 8-bit 4:2:0, no B-frames, a keyframe forced every 2 s exactly (closed
  GOPs), quality 0.68, and `moov` before `mdat`. Pictures bigger or faster
  than 4.1 allows are scaled to fit (4K30 → 1920×1080), `Level::Auto` to opt
  out. Checked from the files' own SPS and, where installed, by ffprobe
  (`profile=High level=41 has_b_frames=0`)
- **Encoders**: H.264 through VideoToolbox only (hardware, or Apple's own
  software encoder on a Mac when asked) — never a bundled H.264 encoder; AV1
  through rav1e, now compiled into every `transcode` build. `encode::open`
  refuses by name, like `decode::open`
- **Muxers**: MP4 through the `mp4` crate plus a faststart rewrite (offsets
  shifted, `stco` widened to `co64` past 4 GB); WebM through a small Matroska
  writer of vtome's own with SeekHead, Duration, a cluster per keyframe, and
  Cues. Both read back through vtome's demuxers
- **Colour from the SPS**: the MP4 demuxer reads matrix, range, primaries,
  transfer, and bit depth from the SPS's VUI instead of guessing by size, and
  the encoders write them in. `scale` converts NV12/I420 and shrinks by area
  averaging
- **One clock for all playback, from atome's cpal stream**: every clip follows
  one engine timeline through `clock::Follower`; `start(…, Audio)` takes
  `Audio::Off` or `Audio::atome(output.clock())`. `Clip::start_at` puts a
  film's first frame at a point on that timeline
- **The engine's surface as sketched**: `add_generic` (decides still or video
  by content), `add_cached` with `cache_frames` (cached ranges from memory,
  decoding only between them), `is_running`, `is_finished`, `snapshot(clip)`
  (the output is `snapshot_output`), `position`, and `Started::monitors`.
  `VideoSource` seeks to the exact frame and knows its `origin`
- **VP9 scaffolding removed**; HEVC and VP9 are refused as out of scope, not
  as a missing feature
- **The Docker matrix stays out of `make test`**: its Mac leg now builds into
  `target/docker-macos`. Sharing `target/` had left test binaries pointing at a
  deleted temp copy, which made `mp4_timing` fail in the normal suite
- `make test` now runs `render,decode-platform,window,split-audio`: 272 tests, all passing
- **No bundled H.264 or AAC codec anywhere** (asked for 2026-10-03). Every
  H.264 encoder and decoder is the OS's: VideoToolbox, and now **Media
  Foundation** (Windows: a vendor's hardware MFT or Microsoft's software one,
  synchronous and asynchronous MFTs behind one `Transform`) and **MediaCodec**
  (Android: `libmediandk` declared by hand, API-26/28 calls looked up at run
  time). Both encode to §15 — High 4.1, no B-frames, 2 s closed GOPs, quality
  rate control — and decode to NV12. Compiled, type-checked, and clippy-clean
  for `x86_64-pc-windows-msvc` and `aarch64-linux-android`; **not yet run on
  either system**. AAC goes through atome's new OS decoders. `cargo tree -i`
  finds no AAC or H.264 codec crate in vtome or atome with every feature on
- **H.264 is the default everywhere.** `import` no longer falls back to AV1
  where the OS has no H.264 encoder; it refuses and says AV1 must be asked
  for. The AV1 decoder stub refuses by name instead of opening and showing
  nothing
- **A video's sound plays through atome, automatically.** `Audio::atome(&output)`
  puts every clip on the output's clock and plays each video clip's
  soundtrack through it — its own audio track, or `Clip::audio(path)` when it
  has none — scheduled to start with its first picture, fed at most 750 ms
  ahead, as one atome voice: stopping the clip takes the sound back within a
  buffer. Looping films loop their sound
- `bitstream::AccessUnit` takes an encoder's Annex B output apart into MP4's
  shape (parameter sets lifted out, length-prefixed slices)
- README rewritten around what exists: the codec rule, `import`, the engine and
  its sound, the feature map, and the Docker matrix as opt-in
