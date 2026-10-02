# vtome — Roadmap

Video Translucent Optimized MacGyver Engine: put an image or a video on a
specific monitor — or a specific *quadrilateral* of a specific monitor — and get
there without FFmpeg and without a codec anyone charges for.

Audio is out of scope on purpose. `atome` is the audio engine; vtome exposes a
clock it can be slaved to (§9) and never opens an audio device itself.

```text
file.mp4 ──demux──▶ H.264 ──platform decode──▶ ┌─────────┐ ──▶ wgpu texture ──▶ corner-pinned quad
                                               │  Frame  │                      on monitor 2
file.webm ─demux──▶ AV1 ────dav1d/rav1d──────▶ │ (YUV +  │
                                               │  color) │ ──▶ rav1e ──▶ AV1 in WebM (transcode)
image.png ──────────────image────────────────▶ └─────────┘
```

Ordered roughly by what unblocks what.

**Where things stand.** Identification, both demuxers, the frame and colour
model, the geometry, placement, the GPU renderer and compositor, the window, the
clock, H.264 decoding through VideoToolbox, and the `Vtome` engine in both its
versions are built and tested — GPU tests included, which draw and read the
pixels back. What is *not* built: H.264 decoding off Apple platforms, AV1
decoding (§2), and encoding (§4). So vtome today puts stills, and on a Mac H.264
video, in any convex quadrilateral on any monitor, in layers, from a
standalone program or from inside a Tauri application.

---

## The decisions this plan rests on

Written down first because everything else follows from them, and because each
one is the answer to "what is my best option across iOS, Android, Windows,
macOS, and Linux".

**Codec scope: decode H.264 and AV1, write AV1. Nothing else** (decided
2026-09-24). H.264 is what arrives; AV1 is what vtome displays and saves. HEVC
and VP9 are out of scope in both directions, so the VP9 fallback this plan once
carried is gone.

**AV1 is the format vtome writes.** AOMedia royalty-free — no per-unit licence,
no H.264 patent pool — and `rav1e` encodes it in **pure Rust** (nasm only for
the optional assembly), so a transcode build needs no C toolchain.

**Decoding input is the hard half, and the OS is the way through it.** Reading
H.264 without FFmpeg means one backend per platform: VideoToolbox (macOS, iOS),
Media Foundation / D3D11VA (Windows), MediaCodec (Android), VA-API or V4L2-M2M
(Linux). That is five backends — but they are hardware-accelerated, they ship
with the OS, and the patent licence is the OS vendor's, not ours. Behind them
sits a portable software AV1 decoder — dav1d/rav1d — so vtome can always read
*its own* format everywhere, even where the OS offers nothing.

**H.264 input on Linux is the one open licensing question.** VA-API covers it
where the driver does; `openh264` compiled from source is the fallback, and
compiling it yourself is *not* the same as Cisco's royalty-covered binary. Decide
deliberately (§11), and never *encode* H.264 — that is what this project exists
to avoid.

**No FFmpeg has a price, and it is paid here:** no free demuxer zoo, no swscale,
no filters. So scaling, colour conversion, and deinterlacing all happen on the
GPU in shaders (which is where they belong anyway), and the list of readable
containers is the list we write (§1).

| Purpose | Crate | Pure Rust? | Notes |
| --- | --- | --- | --- |
| Windowing & monitors | `winit` | wraps the OS | Optional — feature `window` |
| GPU rendering | `wgpu` | mostly | Vulkan/Metal/DX12/GL, iOS + Android |
| Surface handoff | `raw-window-handle` | yes | How Tauri/embedders hand us a surface |
| Image loading | `image` | yes | PNG, JPEG, WebP, GIF, BMP, TIFF |
| MP4 demux | `mp4` | yes | Also carries AV1 (`av01`) |
| MKV/WebM demux | `matroska-demuxer` | yes | `webm` crate is libwebm bindings — prefer the Rust one |
| WebM mux | `webm-iterable` | yes | Writing what §4 encodes |
| Video decode (input) | platform APIs | no | VideoToolbox / MediaFoundation / MediaCodec / VA-API |
| AV1 decode | `dav1d-rs`, or `rav1d` | C / Rust port | `rav1d`'s release state needs checking before we depend on it |
| AV1 encode | `rav1e` | yes | The reason AV1 is the default |

---

## 0. Foundation

- [x] `cargo init --lib`, edition 2021, MIT to match the sibling crates
- [x] `Cargo.toml`: `description`, `keywords`, `categories`, and a dependency
      list where **every heavy thing is optional**. A build that only decodes
      AV1 and hands frames to someone else's renderer must not pull winit, wgpu,
      or a C toolchain
- [x] The feature map, decided before any of it is written, since it is the
      thing that is painful to change later

```toml
default         = ["demux", "decode-av1", "image"]
demux           = ["mp4", "matroska-demuxer"]      # containers
decode-av1      = []                               # dav1d or rav1d
decode-vp9      = []                               # libvpx
decode-platform = []                               # the OS decoder for this target
encode-av1      = ["rav1e"]                        # transcode target
encode-vp9      = ["vpx-sys"]                      # transcode fallback
mux             = ["webm-iterable"]
render          = ["wgpu"]                         # GPU present, no window of its own
window          = ["render", "winit"]              # vtome opens its own windows
embed           = ["render", "raw-window-handle"]  # someone else's surface
image           = ["dep:image"]                    # stills
transcode       = ["demux", "mux", "encode-av1"]
all-decoders    = ["decode-av1", "decode-vp9", "decode-platform"]
```

- [x] `README.md` describing what exists, not what is planned
- [x] Error type: `Unsupported`, `UnknownContainer`, `NoDecoder` (naming the
      remedy), `Demux`, `Decode`, `Encode`, `BadFrame`, `NoSuchMonitor`,
      `Placement`, `Render`, `Io` (carrying the path)
- [x] `Makefile`: `test`, `test-core`, `check` across the feature matrix,
      `clippy`, `doc`, and one target per example (`show`, `monitors`,
      `identify`, `corner-pin`)
- [ ] CI that builds and tests on macOS, Windows, and Linux, and
      *cross-compiles* for `aarch64-apple-ios` and `aarch64-linux-android`. The
      platform decode backends are `cfg`-split, so nothing but CI will notice
      one of them stopped compiling
- [ ] `cargo doc` warnings as errors, since the docs will make platform claims

## 1. Input: identify and demux

- [x] Identify container and encoding from magic bytes rather than the
      extension, as `atome::import` does for audio — same shape, so the two
      crates read alike. ISOBMFF is split by brand, so AVIF and HEIF are told
      apart from film; RIFF by form type, so an AVI is not a WebP
- [x] `MediaInfo` and `TrackInfo`: container, encoding, dimensions, exact
      frame-rate ratios, bit depth, colour, rotation, duration, seekability
- [x] MP4/MOV demux (`mp4`), with the parameter sets rebuilt into an `avcC`
      record so `extra_data` means one thing across containers
- [x] MKV/WebM demux (`matroska-demuxer`), carrying the colour metadata
      Matroska actually states rather than falling back to the guess
- [x] Annex-B and length-prefixed bitstream forms, and the conversion both ways,
      including three- and four-byte start codes and a refusal for a NAL too
      large for its length field
- [x] Ignore audio tracks entirely, but *report* that they exist so a caller can
      hand the file to atome for the audio half
- [x] Keyframe index for seeking, built lazily and kept — the `mp4` crate does
      not expose sample timing without the bytes, so the scan is remembered
      rather than repeated
- [x] Still images through `image` (§8) behind the same front door
- [ ] Fragmented MP4, which `read_header` handles but the sample cursor here
      does not walk

## 2. Decode

One trait, several backends, chosen at runtime by what the platform and the
build actually have.

- [x] `Decoder` trait: `decode(packet) -> Option<Frame>`, `flush`, `reset`, and
      an honest `is_hardware()`
- [x] Backend selection: platform decoder first, software second, and an error
      naming the missing feature — and distinguishing "you did not compile it"
      from "this machine does not have it", which are different problems
- [ ] **macOS/iOS** — VideoToolbox. H.264 is done (see CHANGELOG). Left:
      - [ ] AV1 in hardware on the chips that have it (M3 and later, A17 Pro
            and later): an `av1C` path to a format description. Everywhere
            else AV1 is dav1d's job, below
      - [ ] Zero-copy: the `CVPixelBuffer`'s IOSurface as a Metal texture
      - [ ] Recover from `kVTInvalidSessionErr` (sleep/wake, GPU switch) by
            reopening the session at the next keyframe
      - [ ] Matroska H.264 with B-frames reorders by a fixed four-frame window;
            reading `max_num_reorder_frames` from the SPS would make it exact
- [ ] **Windows** — Media Foundation / D3D11VA through the `windows` crate.
      Output is a D3D11 texture; wgpu's DX12 backend needs it shared, so this is
      the interop that will take the longest
- [ ] **Android** — MediaCodec through `ndk`. Decode to a `SurfaceTexture` and
      sample it as an external texture; never read frames back to the CPU
- [ ] **Linux** — VA-API (`cros-libva`) with a V4L2-M2M path for ARM boards, and
      a documented "software only" outcome where neither exists
- [ ] **Portable software AV1** — dav1d via `dav1d-rs`, or `rav1d` if its
      release state holds up. This is the floor: it is what makes "vtome can
      always play what vtome wrote" true on every target. `decode_av1.rs` is a
      stub today that returns no frames, so `decode-av1` currently opens AV1
      files and shows nothing — it should refuse until it decodes
- [ ] Remove the VP9 scaffolding now that VP9 is out of scope: the
      `decode-vp9`/`encode-vp9` features, `decode_vp9.rs`, and
      `Backend::LibVpx`. HEVC and VP9 should be refused as "not in vtome's
      scope" rather than "not implemented yet"
- [ ] Threading: decode off the render thread, bounded frame queue, backpressure
      rather than unbounded memory
- [ ] Decoder capability query, so an application can ask *before* opening a file
      what this device will manage in hardware

## 3. The frame

- [x] `Frame`: planes (I420, I422, I444, NV12, P010, RGBA/BGRA), strides,
      dimensions, PTS, and full colour metadata. Every layout is validated
      against the buffer, so a header claiming a stride that walks off the end
      is an error rather than a read past it
- [x] Frames stay in YUV. Converting to RGB on the CPU is the single biggest
      waste available to us, and the shader does it for free (§5)
- [ ] `FrameRef` enum: CPU planes, or a GPU handle already in VRAM
      (`CVPixelBuffer`, D3D11 texture, `SurfaceTexture`, `wgpu::Texture`) — the
      type that makes zero-copy expressible rather than accidental
- [x] Frame pool with reuse, so steady-state playback allocates nothing. It
      refuses to reclaim a buffer anything else still holds, so reuse is an
      optimisation and never a race
- [ ] 10-bit and HDR metadata carried through even where §5 tone-maps it away
      for now. `P010` is modelled and the renderer refuses it by name

## 4. Transcode: get everything into a format nobody bills for

- [ ] `transcode(input, output, Settings)` — demux, decode, encode, mux, with no
      temporary files and no full-file buffering
- [ ] AV1 via `rav1e`: CRF/quantizer, speed preset, tiles, threads, keyframe
      interval. Defaults that are sane for playback rather than for archival
- [ ] Mux to WebM (`webm-iterable`) as the native container; AV1-in-MP4 as an
      export option, since that is what more players open
- [ ] Progress callback and cancellation. A 4K transcode is minutes to hours and
      must be interruptible — the same shape pfac wants for bundling
- [ ] Resolution and frame-rate change on the way through, done on the GPU when
      a GPU is present and in a small pure-Rust scaler when it is not
- [ ] Pass-through: an input already AV1 is remuxed, not re-encoded.
      Re-encoding what is already fine is the most common wasted hour in video
- [ ] Hardware *encode* is deliberately not in scope yet: quality is worse, and
      the platform matrix doubles. Revisit only with a measured reason
- [ ] Copy audio tracks through untouched when remuxing, without decoding them —
      the one place vtome touches audio bytes, and it never interprets them

## 5. Present: wgpu, colour, and arbitrary quads

Rendering is a feature (`render`) and does not imply a window. This is the layer
a Tauri app or a game engine borrows without taking winit with it.

- [x] `Renderer` over any `wgpu::TextureView` — a window's, an embedder's, or an
      offscreen one — plus `render_to_rgba` for thumbnails, exports, and tests
- [x] YUV→RGB in the fragment shader, driven by the frame's colour metadata:
      BT.601/709/2020, limited vs full range, planar and bi-planar. One shader,
      uniforms for the rest
- [ ] Zero-copy import per platform: `CVPixelBuffer`→Metal, D3D11→DX12 shared
      handle, `SurfaceTexture`→external texture on Android. Falls back to an
      upload where interop is missing, and says which one it used
- [x] **Corner-pinned output — the trapezoid requirement.** Done exactly as
      planned, and better: the shader covers the whole target with one triangle
      and maps each *pixel* back through the inverse homography, so there is no
      seam because there is no diagonal. A GPU test asserts the picture's midline
      lands within three pixels of where the maths says, on three different rows
      of a keystone
- [x] Reject a non-convex or self-intersecting quad with `Error::Placement`
      rather than drawing something folded — at configuration time, before a
      window is even opened
- [ ] Edge antialiasing on the pinned quad, and optional soft-edge feathering —
      the same knob edge-blended projector arrays need
- [x] Fit modes inside the quad: stretch, contain, cover, and a pixel-exact mode
- [x] Opacity, and transparency outside the quad so a trapezoid shows what is
      behind it rather than a black box — "translucent" is in the name
- [ ] **Composite rendering — multiple layers to one output:**
      - [ ] **Phase 1:** Background layer — color or image
      - [ ] **Phase 1:** Layer compositing — z-index ordering, blend modes
      - [ ] **Phase 1:** Per-layer clipping (for odd shapes via plugins)
- [ ] Frame pacing to the display's refresh: present by PTS against the
      compositor's clock, not by sleeping for `1/fps`

## 6. Placement: which monitor, and where on it

- [x] `Monitor` listing: name, physical position and size in the virtual desktop,
      scale factor, refresh rate in millihertz so 59.94 stays 59.94
- [x] `Placement`: a monitor selector (primary, index, name, or "the one
      containing this point"), an area (full screen, a rect, or a corner quad),
      a fit, an opacity, and always-on-top
- [x] Monitor selectors survive a monitor being unplugged: resolved at apply
      time, falling back to the primary and *saying so* through
      `ResolvedPlacement::fell_back` — or refusing, if the caller marked the
      monitor required
- [x] Fullscreen-on-a-named-monitor, borderless-window-on-a-rect, and
      always-on-top as separate, composable choices
- [x] DPI: physical pixels throughout. Logical pixels across a mixed-DPI
      multi-monitor desktop are a bug generator
- [x] Document plainly that **iOS and Android have no monitor concept** — there
      is one surface, the placement API degrades to "fill it", and external
      displays there are a later item (§11)

## 7. Windowing, and handing off to someone else's window

The `window` feature is the whole point of the split: vtome must be usable from
a Tauri + React app that owns its own window, and equally able to open its own.

- [x] `window` feature over winit: `Viewer` opens an undecorated, transparent
      window, placed per §6, and runs until Escape or close
- [ ] Click-through windows, and several windows at once — `Viewer` shows one
      picture in one place, which is the "put that there" path rather than a
      window manager
- [ ] Shaped windows where the OS allows it, so a trapezoid does not have to sit
      inside a black rectangle
- [x] `embed` feature: `Gpu::from_instance` takes the host's instance and
      surface, and `Renderer::draw` takes any view. This is the Tauri path — no
      per-frame copy, and winit is never compiled
- [ ] **Phase 1:** UI overlays — close button, dragging, resizing rendered sections.
      Texture coordinates for mouse interaction
- [ ] A worked Tauri example, rather than the README's description of one
- [ ] Tauri/TypeScript handoff, documented with a working example:
      - preferred: a native child surface positioned under/over the webview,
        vtome rendering into it directly
      - acceptable: vtome renders to a `wgpu::Texture` the host composites
      - explicitly *not* recommended: shipping decoded frames over IPC to a
        canvas. Write down the number — a 4K frame is ~12 MB, 24 fps is
        ~300 MB/s through a JSON bridge — so nobody rediscovers it the slow way
- [ ] A command/event surface a JS front end can drive: load, play, pause, seek,
      set placement, set corners, set opacity — one small serialisable enum, so
      Tauri commands are a thin wrapper rather than a parallel API

## 8. Still images

- [x] `image` for PNG, JPEG, WebP, GIF, BMP, TIFF, arriving as a `Frame` and
      taking the same placement and corner-pin path as video — a photo on a
      trapezoid is the same code as a film on one. Full-range sRGB, so a still
      is not drawn with video's limited-range pedestal
- [x] A pixel ceiling checked against the *header*, so an absurd scan is refused
      before its pixels are allocated
- [ ] Animated GIF and animated WebP as frame sources, so they play rather than
      showing frame one
- [ ] Very large images: tile or downscale on load rather than handing the GPU a
      texture past `max_texture_dimension_2d`. The renderer refuses one by name
      today; it does not yet do anything cleverer

## 9. Playback, clock, and sync

- [x] `Clock`: play, pause, seek, rate, and a position that survives all of them
      — including pausing twice, which is where a naive anchor rewinds
- [x] A `MasterClock` trait, so atome's audio clock can be the master and vtome
      slaves video to it. **Never the reverse** — audio glitches are audible,
      dropped frames are not
- [x] `Pacing`: present, wait, or drop against the master clock, with counters
      and a drop rate exposed for diagnosis
- [x] **Phase 1a:** `VideoSource` — demuxer, decoder, clock, frame queue, and plugin chain
      per source. Takes demuxed packets, outputs frames to layers
- [x] **Phase 1a:** `OutputLayer` — a VideoSource with position, size, z-index, and
      layer-specific plugins. Renders to an intermediate texture
- [x] **Phase 1a:** `Output` / `Scene` — collection of layers, background color/image,
      composite renderer. Renders layers to final output texture
- [x] **Phase 1a:** `Compositor` — manages multiple VideoSources and Outputs, coordinates
      their frame delivery and timing
- [x] **Phase 1b:** Integration tests — load → demux → decode → composite pipeline
      tested with test fixtures (checkerboard, gradient frames)
- [x] **Phase 1c:** Full playback test — end-to-end composition with multiple outputs,
      layer ordering, opacity, background colors. All 154 tests passing.
- [x] `Player` (higher-level) — tying a single VideoSource to an Output with UI
      (close button stub). Wraps Compositor for simple cases
- [ ] Seek: to the keyframe from §1's index, then decode-and-discard to the
      exact frame
- [ ] Gapless transition between two files, and A/B crossfade, since this exists
      to feed a show-control application
- [ ] Multi-output sync: several monitors/outputs showing several videos that must
      start on the same frame

## 10. Testing

- [ ] **A Docker image per OS that runs the default toolkit's tests**, each
      carrying the converters — ffmpeg with x264, AAC, MP3, Opus — so fixtures
      are made inside rather than skipped (asked for 2026-09-30). **In
      progress.** Docker runs Linux, so: Debian, Ubuntu, Fedora, and Alpine in
      containers; Windows cross-built in a container and tested under Wine;
      Android cross-built with the NDK; a native Windows Dockerfile for a
      Windows host; and macOS and iOS, which cannot be containerised, by a
      script run on a Mac
      - [ ] **Not part of the normal suite** (asked for 2026-10-01): `make test`
            never runs it, and the Mac run builds into a target directory of
            its own. It used to share `target/`, compiled from a temp copy of
            the source, and left test binaries behind pointing at fixture
            paths that no longer existed — `make test` then failed
            `mp4_timing` until something forced a rebuild — **In Progress**
- [x] Fixtures written by the test rather than committed, where the format
      allows it — a PNG saved by the same library that reads it cannot drift
      from what the decoder expects
- [ ] Real video fixtures in `tests/data`, which need §4 to produce them
- [ ] Round trip: transcode a known input to AV1, decode it back, compare PSNR
      against a threshold rather than byte-for-byte
- [ ] A decode test per backend, skipped-with-a-reason rather than failed where
      the platform has no such decoder
- [x] Corner-pin correctness without a GPU: the corners map exactly, the inverse
      round trips, a parallelogram has a zero perspective row and a keystone does
      not, and the centre lands on the diagonal crossing rather than the average
      of the corners
- [x] Headless render tests through wgpu: orientation, transparency outside the
      quad, opacity, planar YUV conversion, and a keystone measured row by row
      against the homography. Skipped with a message where there is no adapter
- [x] Hostile inputs so far: a truncated `avcC`, a parameter set claiming more
      bytes than the record holds, a NAL length running past the data, a plane
      whose stride walks off the end of its buffer, a layout that overflows
      `usize`, and an image past the pixel ceiling
- [ ] The rest: a lying frame count, a resolution past the GPU's texture limit,
      a container whose index disagrees with its own headers
- [ ] Memory ceiling: play a 4K file and assert the frame pool stops growing

## 10.5. Plugins and Effects

Plugins modify video feeds at the frame level or layer level, enabling effects,
custom rendering (odd shapes), masking, and transformations.

- [x] **Phase 1a:** `Plugin` trait — takes a frame, returns a frame
      - [ ] Applied in-order per VideoSource
      - [ ] Examples: rotation, scaling, color correction (implementations later)
- [ ] Layer-level plugins — applied after composition for final polish
      - [ ] Odd-shaped rendering (circles, polygons, beziers) via clipping/masking
      - [ ] Blur, feathering, shadow effects
- [ ] Plugin chaining — multiple plugins per source or per layer
- [ ] Plugin parameters and configuration API
- [ ] A plugin for corner-pinning shapes beyond rectangles (trapezoids, circles)

## 10.6. Compositor and Output Scenes

Multi-source, multi-layer rendering like OBS.

- [ ] **Phase 1:** `VideoSource` — input file/device, demuxer, decoder, clock,
      persistent ID, plugin chain
- [ ] **Phase 1:** `OutputLayer` — references a VideoSource, with position, size,
      z-index, visibility, opacity, blend mode, and layer plugins
- [ ] **Phase 1:** `Output` / `Scene` — collection of layers, background color
      or image (default black), target (monitor or texture). Composite renderer
- [ ] **Phase 1:** `Compositor` — manages multiple VideoSources and Outputs,
      coordinates frame delivery and timing across sources. Returns composite texture
- [ ] Output configuration serialization (save/load scenes)
- [ ] Layer ordering and visibility toggling
- [ ] Background image support (not just color)
- [ ] Blend modes per layer (normal, add, multiply, screen, etc.)
- [ ] Real-time layer manipulation — add/remove/reorder during playback

## 11. Decisions still open

- [ ] **H.264 input on Linux.** VA-API where the driver has it; otherwise
      `openh264` built from source, whose patent position is not Cisco's binary
      licence. Write the conclusion down in the README rather than leaving it
      implied
- [ ] **`rav1d` vs `dav1d-rs`.** The Rust port removes the C toolchain from every
      build, which is worth real effort — but only if its releases and its
      performance are there. Benchmark both before committing
- [ ] Whether vtome ever writes MP4 itself or only WebM
- [ ] HDR: tone-map to SDR at first, or carry PQ/HLG through to a display that
      can take it
- [ ] External displays on iOS and Android, which do exist and are nothing like
      a desktop monitor list

## 12. Later

- [ ] **RTSP output** — stream Output to RTSP server (separate from monitor output).
      Requires encoder (§4) and network mux. Phase 2 after single-monitor MVP
- [ ] Hardware encode, if §4's measurement ever justifies it
- [ ] Network sources: HTTP range requests, HLS/DASH — vtome reading from a
      `Read + Seek` rather than only a `File`, the same shape pfac wants
- [ ] Playing straight out of a pfac bundle, since `pfac::Bundle::stream` is
      already `Read + Seek + Send` and that is exactly what a demuxer needs
- [ ] Video device input (webcam, HDMI capture) — with persistent IDs for hotplug resilience
- [ ] Capture: screen in, as a frame source
- [ ] Real-time effects between decode and present, as shader passes
- [ ] Deinterlacing, for the archival footage that will inevitably turn up

## 13. `Vtome`: the engine an application drives

Asked for 2026-09-24, in the shape OrbitX already stores its outputs:
`src-tauri/src/config/settings.rs` has `montior_size_out: HashMap<String,
(i64, i64, i64, i64)>` and `video_matrix_in` keyed by vtome ids. The engine
exists in both versions (see CHANGELOG); what is left:

- [ ] **A video's audio goes to atome automatically**, and the video's clock
      slaves to atome's play position through `MasterClock`. Needs atome's play
      position (atome planning §13) and an optional `atome` feature here
- [ ] **`import(input, output, options)`**, a free function rather than a
      method: decode (H.264 through the OS decoder) → encode AV1 with `rav1e` →
      mux (WebM, or AV1-in-MP4 — §11) → write to disk. This is §4's transcode
      with a front door. `rav1e` is software and slow, so §4's progress
      callback and cancellation are part of this, not extras
      - [ ] Option to split the audio out as FLAC for atome — atome's side is
            its planning §3.5 (FLAC writing) and §4 (audio out of video files)
- [ ] **AAC through the OS, never a bundled decoder** (decided 2026-09-26) — the
      same rule as H.264, for the same reason: the patent licence stays the
      OS vendor's. The audio in an MP4 is nearly always AAC. vtome itself never
      decodes audio, so this means `import`'s audio split and a video's audio
      handed to atome both go through atome's OS AAC path (atome planning
      §3.3), and vtome never pulls in a crate or feature that bundles one
- [ ] Confirm by eye that a Tauri overlay is see-through outside the picture on
      macOS. Transparency there is two AppKit calls vtome makes itself (Tauri
      gates `transparent` behind `macos-private-api`); every test so far reads
      the picture back offscreen, which cannot show the window's own alpha
- [ ] Decode off the engine thread. Each output's films decode where they are
      drawn today, which is fine at 1080p and will not be at several 4K layers
- [ ] Hotplug: a monitor unplugged mid-show. Its surface reports `Lost` and is
      reconfigured every frame; the output should close, and reopen when the
      monitor returns, reported through `Controls`

## 14. Import queue, proxies, and one clock — **In Progress**

Asked for 2026-10-01, replacing the "move the default to VP9" note that stood
here. The request, as written:

> Move the default encoding to H264 provide a temporary low quality file that
> can be used in the location of the output file, the better quality file will
> be transcoded in a tmp dir then replace the bad file. add a function that
> relays the progress for all files being worked on, and make sure the
> transcoding is a queue so we dont destroy the cpu spawning a bunch of
> transcoders. bundle av1 encoder with the app.
> add a clock that is used to control the playback and it is based on the
> audio atome cpal clock.

### 14.1 Writing: H.264 by default, AV1 bundled — **In Progress**

- [ ] `encode`: an `Encoder` trait (frame in, packets out, `finish`, an honest
      `is_hardware`) and a backend choice shaped like `decode::open`, so "not
      compiled in" and "not on this machine" read differently
- [ ] **H.264 through the OS encoder, never a bundled one** — VideoToolbox on
      Apple targets. No x264, no openh264: the licence stays the OS vendor's,
      the same rule as decoding H.264 and AAC
      - [ ] Media Foundation (Windows) and MediaCodec (Android), behind the same
            trait, later
- [ ] **AV1 through `rav1e`, compiled into every `transcode` build** — the
      fallback wherever the OS has no H.264 encoder (Linux today) and the
      choice when asked for. Pure Rust, so bundling it costs build time, not a
      toolchain
- [ ] Mux: H.264 into MP4 (the `mp4` crate's writer), AV1 into WebM (a small
      Matroska writer of vtome's own, with Cues so the result seeks). The
      container follows the codec, never the output path's extension
- [ ] §15's universal baseline is the contract for every H.264 file `import`
      writes: High profile, **Level 4.1 declared** (never AutoLevel), 8-bit
      4:2:0, no B-frames, a fixed keyframe every 2 s so every GOP is closed,
      and `moov` before `mdat` — the `mp4` crate writes it last, so the file is
      rewritten faststart once the encode ends. VideoToolbox: frame reordering
      off, quality 0.68 (bitrate where the encoder will not take a quality)
      - [ ] 4.1 holds 8,192 macroblocks a frame and 245,760 a second — 1080p30,
            or 720p60. A picture bigger or faster than that is scaled down to
            fit rather than declared at a level older decoders refuse; a caller
            who wants 4K asks for it (`Level::Auto`)
- [ ] A small pure-Rust scaler and NV12/I420 conversion — the proxy is
      downscaled, and rav1e takes planar input
- [ ] Colour survives the trip: the encoder is told the source's matrix and
      range, and the MP4 demuxer reads them back from the SPS instead of
      guessing by resolution — which would call a 640×360 proxy of a BT.709
      film BT.601
- [ ] The VP9 scaffolding goes (§2's item): `decode-vp9`, `encode-vp9`,
      `decode_vp9.rs`, `Backend::LibVpx`

### 14.2 `import`: a proxy now, the real file later — **In Progress**

- [ ] `import(input, output, output_audio: Option<path>)` returns a job id at
      once, after checking the input opens and something here decodes it — a
      bad file is an error at the call, not a failed job later
- [ ] A low-quality proxy is written at `output` first (downscaled, fastest
      settings, H.264 by default) so the application can use that path
      straight away. The full-quality file encodes in a temp directory and
      then replaces the proxy in one rename — copy-then-rename when the temp
      directory is on another volume — so the path never holds half a file
- [ ] One function reports every file being worked on: stage, fraction done,
      frames, the codec actually used, whether the proxy is ready, and why a
      job failed
- [ ] A queue, not a thread per file: one lane for proxies and one for final
      encodes, so two encoders at most run however many files are imported.
      Proxies have their own lane so a new file is usable without waiting
      behind an hour-long final encode
- [ ] Cancel a job: queued stages are dropped, a running one stops at the
      next frame, temp files are removed
- [ ] Hardware acceleration is an import option (asked for 2026-10-01):
      `Hardware::Prefer` (the default — hardware where the machine has it,
      the OS's software codec where not), `Require` (hardware or a clear
      refusal at `import`), `Off` (software throughout). It reaches both the
      decoder and the encoder, and the progress report says what each one
      actually used. `Require` with AV1 is refused up front — rav1e is
      software
- [ ] `output_audio`: the audio split to FLAC through atome's `to_flac`. Its
      own feature, `split-audio`, because atome's `import` still decodes AAC
      with Symphonia's bundled decoder until atome §3.3 moves AAC to the OS —
      which §13's rule says vtome must not pull in quietly

### 14.3 One clock for all playback, from atome's cpal stream — **In Progress**

- [ ] atome: a play-position clock off the cpal output callback — frames
      handed to the device, less output latency, smoothed between callbacks,
      readable from any thread without a lock (atome §13)
- [ ] vtome: every clip runs against one engine timeline instead of a clock of
      its own. With audio, the timeline *is* atome's clock, so video follows
      audio and never the reverse (§9); without, a monotonic clock
- [ ] `start(outputs, backgrounds, audio)`: `Audio::Off`, or
      `Audio::atome(clock)` with the `atome` feature, or any `MasterClock`
- [ ] `Clip::start_at(position)` — frame zero lands at that point on the
      timeline, which is how a video lines up with audio scheduled in atome
- [ ] A video's own audio still goes to atome by hand; doing it automatically
      is §13's first item

### 14.4 The engine's surface, as sketched — **In Progress**

The sketch that came with the request, and where each call lands. Both hosts —
vtome's own winit windows and a Tauri application's — share it.

| Sketched | In vtome |
| --- | --- |
| `import(input, output, output_audio)` | `vtome::import`, plus `ImportQueue` for a queue of the caller's own |
| `Engine.start(audio_enabled, audio_engine)` → monitors | `start(outputs, backgrounds, Audio)`; `TauriVtome` returns the monitors in `Started`, and `Vtome::start_with` hands them over before choosing outputs (an event loop is the only way to list them, and it runs once) |
| `Engine.add(Clip)` → clip id | `add(Clip)` → `ClipId`, for any file vtome reads — what `import` writes included |
| `Engine.add_generic(video)` | `add_generic(path, monitor)`: a video on a monitor's whole output, with no clip settings |
| `Engine.add_cached(Clip, ranges, frames)` | `add_cached(Clip, ranges, frames)`: frame ranges served from memory, decoding only outside them; `cache_frames` makes the cache |
| `Engine.stop(id)` | `stop(id)` |
| `Engine.is_running(id)` | `is_running(id)` |
| `Engine.is_finished(id)` | `is_finished(id)`: ended, failed, or stopped |
| `Engine.snapshot(id)` | `snapshot(id)`: the clip's current frame. The whole output is `snapshot_output(monitor)` |

- [ ] `is_running`, `is_finished`, `snapshot(id)` / `snapshot_output`
- [ ] `add_generic`, `add_cached` (and `cache_frames`), which needs §9's seek:
      keyframe, then decode-and-discard
- [ ] `Started` carries the monitor list


## 15. Improve encoding:

Your switch to H.264 is the right call for a cross-platform Tauri app, and it dramatically simplifies the architecture you've been building. The patent threshold you're referring to (the MPEG LA pool's 100,000-unit annual threshold) is a real provision, though individual patent holders outside the pool can still assert claims—but for a small or personal app, this is a widely accepted pragmatic position. The far more important consequence is that **H.264 has universal hardware decode support on every platform Tauri targets**, which eliminates the entire class of problems we discussed with AV1.

Below are concrete improvements to your `vtome` code and the surrounding Tauri architecture, organized by layer.

## 1. File Format Compliance (Do This at Encode Time)

The guidance file's "Summary Checklist for Instant Hardware Playback" is the most important part for `vtome`. These are not optional niceties—they directly determine whether hardware decode works and whether seeking is instant.

**Enforce these in your encoder pipeline:**

| Setting | Value | Why It Matters for vtome |
|---|---|---|
| Pixel format | 8-bit YUV 4:2:0 (NV12) | 10-bit or 4:2:2 forces software decode on many SoCs and older GPUs |
| Profile | High @ Level 4.1 max | Higher levels break hardware decode on cheap Android and older iPhones |
| B-frames | 0–1 | Higher counts add decode latency and complicate frame reordering |
| GOP | Closed, 2–3 second keyframe interval | Deterministic seek points; open GOP requires decoding from prior GOP |
| Container | MP4 with `moov` atom at start | Instant start from any timestamp without reading the whole file |

For `moov` at start, use `ffmpeg -movflags +faststart` or the equivalent in your encoder. Without it, `vtome` cannot seek until the entire file has been scanned, which defeats the purpose of a native renderer.

**Closed GOP is especially important for vtome.** If you rely on open GOP (where B-frames can reference frames from the previous GOP), a seek to a keyframe still requires decoding frames from before that keyframe. Closed GOP makes every keyframe a true random-access point, which means `vtome`'s seek logic can be a simple "find nearest keyframe, decode forward" operation rather than a "find keyframe, decode from previous keyframe, discard" operation.

## 2. vtome Decoder Improvements

### Use Hardware Decode Explicitly

Since you control encoding, you can guarantee the file matches the hardware decoder's constraints. Configure your decoder to prefer hardware:

```rust
// Pseudocode for a hardware-decode-first strategy.
// The exact API depends on whether vtome uses FFmpeg, a native decoder,
// or a Rust decoder crate. The principle is the same everywhere.

pub enum DecodeBackend {
    VideoToolbox,  // macOS / iOS
    D3D11VA,       // Windows
    VAAPI,         // Linux (Intel/AMD)
    V4L2M2M,       // Linux ARM (Raspberry Pi, etc.)
    MediaCodec,    // Android
    Software,      // fallback
}

impl Decoder {
    pub fn new(path: &Path) -> Result<Self, DecodeError> {
        let hw_candidates = Self::platform_backends();
        for backend in hw_candidates {
            match Self::try_hw_decode(path, backend) {
                Ok(dec) => return Ok(dec),
                Err(e) => log::warn!("{backend:?} failed: {e}, trying next"),
            }
        }
        Self::software_decode(path)
    }
}
```

The key improvement over a naive `Decoder::new()` is **graceful fallback with logging**. Hardware decode can fail for reasons unrelated to codec support (driver issues, surface format mismatch, memory pressure). You want the player to keep working, not crash.

### Zero-Copy Frame Path to wgpu

Since `vtome` renders with `wgpu`, the biggest performance win is avoiding a CPU round-trip for every frame. Hardware decoders on most platforms can output directly into a GPU-accessible surface:

- **VideoToolbox**: outputs `CVPixelBuffer` with `IOSurface` backing, which can be wrapped as a `wgpu::Texture` via `MTLTexture`.
- **D3D11VA**: outputs `ID3D11Texture2D`, which can be shared with `wgpu` via `D3D12` interop or `wgpu-hal`'s external texture support.
- **VA-API**: outputs `VASurface`, which maps to a DMA-BUF that `wgpu` can import on Linux.
- **MediaCodec**: outputs `Surface`/`Image`, importable via `AHardwareBuffer` on Android.

If `vtome` currently decodes to CPU memory and then uploads to `wgpu`, converting to a zero-copy path is the single largest performance improvement available. It reduces per-frame CPU usage dramatically and eliminates a full-frame memcpy per frame.

```rust
// Conceptual zero-copy upload
impl FrameUploader {
    pub fn upload(&mut self, frame: &DecodedFrame) -> &wgpu::Texture {
        match frame.backing {
            FrameBacking::GpuNative(handle) => {
                // Import the platform handle directly — no CPU copy.
                self.import_native_texture(handle)
            }
            FrameBacking::Cpu(pixels) => {
                // Slow path: upload via queue.write_texture
                self.queue.write_texture(/* ... */, pixels, /* ... */);
                &self.texture
            }
        }
    }
}
```

### Ring Buffer for Decoded Frames

A player that decodes one frame at a time will stutter on any decode spike. Use a bounded ring buffer (e.g., 8–16 frames) between the decode thread and the render thread:

```rust
pub struct FrameRing {
    slots: Vec<Option<DecodedFrame>>,
    head: AtomicUsize,
    tail: AtomicUsize,
    capacity: usize,
}

impl FrameRing {
    /// Called by decode thread. Blocks if full.
    pub fn push(&self, frame: DecodedFrame) { /* ... */ }

    /// Called by render thread. Returns None if empty.
    pub fn pop(&self) -> Option<DecodedFrame> { /* ... */ }
}
```

The guidance file's "0–1 B-frames" and "Closed GOP" settings make this buffer's job much easier, because frame decode order matches presentation order (or nearly so), so you don't need a reordering buffer.

## 3. Architecture Improvements for Tauri Integration

### Separate Decode Thread from Render Thread

Your current architecture likely has one thread owning the `vtome::Viewer`. As the player grows, decode and render compete for the same thread. Split them:

```
┌──────────────────┐   ring buffer   ┌──────────────────┐
│  Decode Thread   │ ──────────────► │  Render Thread   │
│                  │                 │                  │
│  owns Decoder    │                 │  owns winit      │
│  hardware decode │                 │  owns wgpu       │
│  fills ring      │                 │  presents frames │
└──────────────────┘                 └──────────────────┘
        ▲                                     ▲
        │ commands                            │ clock ticks
        │                                     │
┌──────────────────────────────────────────────────────┐
│              Tauri Main Thread / Tokio               │
│  holds: VideoHandle { cmd_tx, status: Arc<...> }     │
└──────────────────────────────────────────────────────┘
```

This mirrors the atome architecture you already have: the decode thread is the "producer," the render thread is the "consumer," and the Tauri side holds only a `Send + Sync` handle.

### Shared Status Struct (Extend What You Already Have)

Your audio `PlaybackStatus` can be extended for video, or you can keep a parallel `VideoPlaybackStatus`. Because you are slaving video to the audio clock (as `vtome`'s `clock` module supports), the video status can read the audio position rather than maintaining its own:

```rust
pub struct VideoPlaybackStatus {
    pub running: AtomicBool,
    pub current_item: Mutex<Option<String>>,
    pub decode_backend: Mutex<DecodeBackend>,  // for diagnostics
    pub frames_dropped: AtomicU64,             // for health monitoring
    pub last_keyframe_ms: AtomicU64,           // for seek feedback
}
```

Adding `frames_dropped` and `last_keyframe_ms` gives your Tauri UI something actionable to display if playback degrades, rather than a binary running/stopped flag.

### Platform-Specific Thread Constraints (Revisited)

The guidance file's encoder notes map to *encoder* backends, but the same platform split applies to *decoders*. The critical architectural constraint remains:

- **macOS / iOS**: `winit` requires the event loop on the main thread. Since Tauri also wants the main thread, you have two options: run `vtome`'s render loop on the main thread and Tauri's event loop via `EventLoopProxy`, or run `vtome` in a separate process. On iOS specifically, this is the hardest platform to integrate.
- **Windows / Linux / Android**: `winit` can run on a secondary thread. The clean actor pattern works without compromise.

If you are targeting iOS, the separate-process approach for `vtome` is worth serious consideration, because fighting `winit` and Tauri for the main thread on iOS is a known source of fragility.

### Proper Shutdown

Your architecture should send `Shutdown` to both the decode thread and the render thread, join both with a timeout, and only then let Tauri exit. The decode thread must drop the decoder explicitly; the render thread must drop the `wgpu` surface and `winit` window in the correct order (surface before device, window last).

```rust
impl Drop for VideoEngine {
    fn drop(&mut self) {
        let _ = self.decode_tx.send(DecodeCommand::Shutdown);
        let _ = self.render_tx.send(RenderCommand::Shutdown);
        let _ = self.decode_join.take().map(|h| h.join());
        let _ = self.render_join.take().map(|h| h.join());
    }
}
```

Do not rely on `Drop` for `wgpu` types across threads—the ordering matters and is platform-sensitive. Explicit shutdown with joins is the reliable pattern.

## 4. Encoder Settings (When You Create Content)

The guidance file gives per-platform encoder settings for *transcoding*. Since you are encoding once and shipping the files, you should pick the most universally compatible output, not the fastest per-platform output. That means:

- **Encode with x264 on your build machine** (software), not with platform hardware encoders. Hardware encoders prioritize speed over quality and produce files that vary by GPU vendor.
- Use `-profile:v high -level 4.1 -pix_fmt yuv420p -bf 0 -g 60 -keyint_min 60 -sc_threshold 0 -movflags +faststart`.
- `-sc_threshold 0` forces a fixed keyframe interval, which produces a **closed, deterministic GOP structure**—exactly what `vtome`'s seek logic wants.
- `-bf 0` gives zero B-frames, which matches the guidance file's "0–1 B-frames" recommendation and simplifies decode ordering.
- `-g 60` at 30fps gives a 2-second keyframe interval, matching the guidance's "2 or 3 seconds."

If you later add on-device export, then the per-platform encoder settings in the guidance file become relevant—but for content you ship, software x264 with these flags is the most predictable choice.

## 5. What Changed vs. the AV1 Plan

Since you switched from AV1 to H.264, several previous concerns disappear:

| Concern | AV1 | H.264 (your new choice) |
|---|---|---|
| Hardware decode on old iPhones | Requires A17 Pro+ | Universal since ~2010 |
| Hardware decode on M1/M2 Macs | Not supported | Universal |
| Software decoder fallback needed | Yes (`dav1d`) | Not needed in practice |
| WebView `<video>` reliability | Fragmented | Universal |
| File size | Smallest | Largest |
| Encode time | Slowest | Fastest |

The trade-off is file size. H.264 files are roughly 60% larger than equivalent AV1 files. For a local-disk app, this is usually acceptable; if storage becomes a concern, you can revisit AV1 later with the native-decode architecture you are already building.

## 6. Concrete Improvement Checklist for vtome

1. **Add `faststart` verification** at load time. If the `moov` atom is not at the start, log a warning and seek to the end to force a scan, or reject the file in development builds so you catch it early.
2. **Prefer hardware decode** with explicit fallback and logging of which backend was chosen.
3. **Implement zero-copy upload** from decoder surfaces to `wgpu` textures where the platform allows.
4. **Add a bounded frame ring buffer** between decode and render threads.
5. **Expose `decode_backend` and `frames_dropped`** in the shared status struct so the Tauri UI can show diagnostics.
6. **Split decode and render threads** so decode spikes do not stall presentation.
7. **Enforce closed GOP and 0 B-frames** in your encoder settings so seek and frame ordering stay simple.
8. **Handle platform thread constraints explicitly**—main-thread `winit` on Apple platforms, secondary-thread on Windows/Linux/Android.
9. **Implement explicit shutdown** with joins for both threads; do not rely on `Drop` ordering across threads.

The guidance file's checklist is fundamentally a decode-compatibility checklist, and every item on it maps to a concrete improvement in `vtome`. Enforcing those at encode time is the cheapest way to make the player's job easy, and the architecture improvements above keep the player responsive and diagnosable once it is running.
Your instinct to build a universal baseline first is exactly right. The core problem with the original spec is that it mixes **file-level compatibility** (what every decoder can play) with **encoder-level tuning** (how to make each GPU go fast). Those are two different layers, and conflating them is what causes playback failures when a file moves between platforms. The fix is to define a single **universal output spec** that every hardware encoder must produce, then tune each encoder's internal settings to hit that spec as fast as possible.

## The Universal Baseline (The Only Spec That Matters for Playback)

This is the contract. Every file you ship—regardless of which hardware encoder produced it—must conform to these parameters. If a file meets this spec, it will hardware-decode on every Tauri WebView and every `winit`/`wgpu` pipeline you target, from a 2012 iPad to a modern Android flagship.

| Parameter | Universal Value | Why This Exact Value |
|---|---|---|
| **Profile** | High | Highest compression efficiency that is still universally hardware-decoded. Main is a fallback only if you encounter a specific legacy device. |
| **Level** | **4.1** | This is the ceiling for universal mobile compatibility. High 4.1 is supported from iPad 2 / iPhone 4s onward, and from the earliest Android hardware decoders. Level 4.0 is slightly safer for very old devices, but 4.1 covers 1080p60 and is the modern floor. |
| **Chroma / Bit Depth** | 8-bit YUV 4:2:0 (`yuv420p`) | Non-negotiable. 10-bit or 4:2:2 instantly forces software decode on Apple Silicon, NVDEC, and most mobile SoCs. |
| **B-Frames** | 0 | **This is the most important cross-platform change from the original spec.** Zero B-frames means decode order equals presentation order. There is no reordering buffer, no decode latency spike, and no risk of a platform's hardware decoder choking on a reference structure it does not expect. The bitrate cost is roughly 5–10%, which is a fair trade for guaranteed hardware decode. |
| **GOP** | Closed, fixed interval (2 seconds) | A closed GOP means every keyframe is a true random-access point. Seeking becomes deterministic: find nearest keyframe, decode forward. No dependence on frames from a previous GOP. |
| **Keyframe Interval** | 2 seconds (60 frames @ 30fps, 120 @ 60fps) | Matches the original spec's "2 to 4 seconds" guidance but fixes it at the lower end. Denser keyframes cost bits but make scrubbing instant and hardware decode startup deterministic. |
| **Entropy Coding** | CABAC | Mandatory in High profile. No decode cost on modern hardware. |
| **Container** | MP4 with `moov` at start (`+faststart`) | Without this, the player cannot seek until it has read the entire file. This is the single most common cause of "it plays but scrubbing is broken." |

**Why 0 B-frames, specifically?** The original spec said "0 to 1 B-frame." That range is too loose for a cross-platform guarantee. Some hardware decoders on older mobile SoCs handle 1 B-frame correctly; others introduce a one-frame latency that causes audio/video desync when you are slaving video to an audio clock (which your `vtome` + `atome` architecture does). Zero B-frames removes that entire class of problem. The encoding speed benefit is a bonus.

**Why Level 4.1 and not 5.2?** Intel QuickSync, NVENC, and VideoToolbox all support levels far above 4.1. But the file's level field is a **declaration of required decoder capability**. If you write Level 5.2 into the bitstream, an older mobile decoder that only supports 4.1 will refuse to play the file even if the actual resolution and frame rate are within its capability. Level 4.1 tells every decoder "this stream stays within 1080p60 and 24 Mbps," which is the broadest safe envelope.

## Platform-Specific Hardware Encoder Tuning (Encoder Side Only)

These settings change **how** each encoder produces the universal output. They do not change the output format. A file produced by any of these profiles is bit-for-bit interchangeable with a file produced by any other, from a playback perspective.

### Apple Silicon / macOS / iOS (VideoToolbox)

| Setting | Value | Reason |
|---|---|---|
| Profile Level | `kVTProfileLevel_H264_High_4_1` | Explicitly declares High 4.1. Do not use AutoLevel—it may select a higher level that older iOS devices reject. |
| `AllowFrameReordering` | `false` | Forces 0 B-frames. This is the Apple hardware encoder's fastest path and guarantees decode-order = presentation-order. |
| `RealTime` | `true` (for interactive export) | Prioritizes throughput over micro-optimization. |
| Quality | `0.65–0.70` (VBR) | The original spec's range is correct. This yields visually clean 1080p at reasonable bitrates. |
| Keyframe Interval | 2 seconds | Matches universal spec. |

VideoToolbox's hardware encoder is on the SoC's media engine and is power-efficient. Setting `AllowFrameReordering = false` is the single most important flag: it disables B-frame generation entirely, which is what makes the output universally compatible and the encoding pipeline maximally fast.

### Windows / Linux Nvidia (NVENC)

| Setting | Value | Reason |
|---|---|---|
| Profile | `high` | Standard. |
| Level | `4.1` | Explicitly set; do not let the driver auto-select. |
| Preset | `p4` (balanced) | `p3` is faster but produces larger files; `p4` is the quality/speed sweet spot. |
| B-Frames | `0` | Critical. Set `-bf 0` explicitly. |
| Lookahead | `0` (disabled) | The original spec's "0–4 frames" is too permissive. Zero lookahead removes the multi-pass delay and is the fastest path. |
| Rate Control | `vbr_hq` | High-quality VBR gives better quality at a given bitrate than CQP for content with varying complexity. |
| Multipass | `0` (disabled) | A single pass is sufficient for the universal spec. |

NVENC's H.264 encoder is available on all GPUs from Kepler onward and is the most broadly deployed hardware encoder on Windows and Linux. Zero B-frames and zero lookahead are the two settings that take it from "fast" to "instant" without affecting the output's compatibility.

### Intel QuickSync (QSV)

| Setting | Value | Reason |
|---|---|---|
| Profile | `high` | Standard. |
| Level | `4.1` | QSV hardware supports up to 5.2, but 4.1 is the universal ceiling. |
| Preset | `fast` or `faster` | The QSV preset vocabulary maps to x264-style names; `fast` is the right balance. |
| Rate Control | `icq` (`-global_quality N`) | ICQ is libmfx's recommended single-pass perceptual quality mode and is the closest analogue to x264's CRF. Use a quality value around 23–25. |
| B-Frames | `0` | Force zero. QSV's `GOPRefDist = 1` achieves this. |
| Lookahead | `0` | Disable. QSV lookahead can behave inconsistently across driver versions. |

QSV is available on Intel iGPUs from 7th-gen (Kaby Lake) onward and on Arc GPUs. It is the most broadly available hardware encoder on Intel-based Windows machines and Linux desktops.

### Android (MediaCodec)

| Setting | Value | Reason |
|---|---|---|
| Profile | `AVCProfileHigh` | High profile is supported on Android 10+; for older devices, `AVCProfileMain` is the safe fallback. |
| Level | `AVCLevel41` | Level 4.1 is the universal mobile ceiling. |
| Color Format | `COLOR_FormatYUV420SemiPlanar` (NV12) | Mandatory. No other format will hardware-decode reliably. |
| Bitrate Mode | `BITRATE_MODE_VBR` | VBR gives the best quality/size trade-off. |
| I-Frame Interval | `2` (seconds) | Matches universal spec. |
| B-Frames | Not set / 0 | Android's MediaCodec B-frame support is inconsistent. The safest path is to not configure B-frames at all, which defaults to zero on most encoders. |

Android's mandatory H.264 support is **Main Profile Level 3.1 and Baseline** for compatibility. High 4.1 is a superset that modern devices support, but if you want absolute maximum reach on very old Android devices, a second output at Main 3.1 is the only way to guarantee it. For iOS and modern Android, High 4.1 is the correct target.

### Linux ARM / x86 (VA-API / V4L2)

| Setting | Value | Reason |
|---|---|---|
| Profile | `high` | Standard. |
| Level | `4.1` | Universal ceiling. |
| Surface Format | `NV12` | Mandatory for hardware decode. |
| Rate Control | `CQP` or `VBR` | CQP is simpler and predictable; VBR is better for variable content. |
| B-Frames | `0` | Force zero. VA-API B-frame support varies by driver. |
| GOP | Closed, 2-second interval | Matches universal spec. |

VA-API is the standard hardware encode path on Linux for Intel and AMD GPUs. On ARM (Raspberry Pi, Rockchip, etc.), V4L2 M2M is the equivalent. Both require NV12 surface format and zero B-frames for reliable hardware decode on the output side.

## The Cross-Platform Guarantee Matrix

This table answers the core question: if I encode on platform X, will it play on platform Y?

| Encoded On → | Apple | Windows | Linux | Android | iOS |
|---|---|---|---|---|---|
| **Apple (VideoToolbox)** | ✅ | ✅ | ✅ | ✅ | ✅ |
| **Nvidia (NVENC)** | ✅ | ✅ | ✅ | ✅ | ✅ |
| **Intel (QSV)** | ✅ | ✅ | ✅ | ✅ | ✅ |
| **Android (MediaCodec)** | ✅ | ✅ | ✅ | ✅ | ✅ |
| **Linux (VA-API)** | ✅ | ✅ | ✅ | ✅ | ✅ |

This is the entire point of the universal spec. Because every encoder is forced to produce High 4.1, 8-bit 4:2:0, zero B-frames, closed GOP, with `moov` at start, the output files are **structurally identical from a decoder's perspective**. The only differences are bitrate and quality, neither of which affects hardware decode eligibility.

## What Changed from the Original Spec

The original spec was directionally correct but had two flaws that would cause problems in practice:

1. **"0 to 1 B-frames"** → Changed to **0 B-frames**. The ambiguity means different encoders will make different choices, and a 1-B-frame file from one platform may not hardware-decode identically on another. Zero removes the ambiguity and the decode-reordering complexity.

2. **Level 4.1 "or 4.0"** → Changed to **4.1, fixed**. The "or" creates a split: some files at 4.0, some at 4.1. Level 4.1 is the correct universal target. 4.0 is only needed for very old devices (pre-iPhone 4s), which are not a meaningful target for a modern Tauri app.

Additionally, the platform-specific sections in the original spec were framed as "how to encode for this platform." The revised framing is "how to make this platform's encoder produce the universal file." That shift matters because it ensures cross-platform interchangeability is the primary constraint, not per-platform speed.

## Practical Implementation in Your Build Pipeline

In your FFmpeg-based export pipeline, the universal spec translates to a single command template with platform-specific encoder selection:

```
# Universal spec flags (always applied)
-profile:v high -level 4.1 -pix_fmt yuv420p -bf 0 -g 60 -keyint_min 60 -sc_threshold 0 -movflags +faststart

# Platform encoder selection
-c:v h264_videotoolbox   # Apple
-c:v h264_nvenc          # Nvidia
-c:v h264_qsv            # Intel
-c:v h264_mediacodec     # Android
-c:v h264_vaapi          # Linux
```

The `-sc_threshold 0` flag forces a fixed keyframe interval, which is what creates the **closed, deterministic GOP** structure. Without it, the encoder may insert scene-change keyframes that break the 2-second cadence and complicate seek logic. The `-g 60 -keyint_min 60` pair pins the GOP size to exactly 60 frames at 30fps.

If you are encoding on-device (e.g., an Android export feature), you will need to set these through the platform's native API (MediaCodec `MediaFormat`, VideoToolbox `VTCompressionSession`, etc.) rather than FFmpeg flags, but the values are the same.

## Summary

| Layer | What It Controls | Value |
|---|---|---|
| **Universal output spec** | Whether a file plays on any platform | High 4.1, 8-bit 4:2:0, 0 B-frames, closed GOP, 2s keyframes, `moov` at start |
| **Apple encoder tuning** | How fast VideoToolbox produces that output | `AllowFrameReordering = false`, quality 0.65–0.70 |
| **Nvidia encoder tuning** | How fast NVENC produces that output | Preset p4, 0 lookahead, 0 B-frames, VBR_HQ |
| **Intel encoder tuning** | How fast QSV produces that output | Preset fast, ICQ mode, 0 B-frames |
| **Android encoder tuning** | How fast MediaCodec produces that output | NV12, VBR, 2s I-frame interval |
| **Linux encoder tuning** | How fast VA-API/V4L2 produces that output | NV12, CQP/VBR, 0 B-frames |

The rule is simple: **the universal spec is the contract; the platform tuning is how you fulfill it quickly**. As long as every encoder emits the same structural format, you get cross-platform playback for free, and you can pick whichever hardware encoder is available on the machine doing the encoding without worrying about whether the output will play elsewhere.