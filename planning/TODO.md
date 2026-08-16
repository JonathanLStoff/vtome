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

**Where things stand.** Everything up to the pictures themselves is built and
tested: identification, both demuxers, the frame and colour model, the geometry,
placement, the GPU renderer, the window, and the clock — 127 tests, including
GPU tests that draw a keystoned quad and read the pixels back. What is *not*
built is the decoders (§2) and the encoders (§4): §2 is the trait, the backend
selection, and an error naming the backend that would have taken the work,
deliberately rather than a decoder that returns no frames. So vtome today shows
**still images** in any convex quadrilateral on any monitor, and knows everything
about a video file except how to turn its packets into pictures.

---

## The decisions this plan rests on

Written down first because everything else follows from them, and because each
one is the answer to "what is my best option across iOS, Android, Windows,
macOS, and Linux".

**AV1 is the format vtome writes. VP9 is the fallback it also writes.** Both are
AOMedia/Google royalty-free — no per-unit licence, no H.264/HEVC patent pool.
AV1 is the better bet of the two: `rav1e` encodes it in **pure Rust** (nasm only
for the optional assembly), where VP9 encoding means libvpx and a C toolchain on
five platforms. So the pure-Rust column of the original table gets *better* by
choosing AV1 first, not worse. VP9 stays as the compatibility answer, since
hardware VP9 decode is on nearly every Android phone and Apple Silicon Mac while
hardware AV1 decode is only on recent GPUs and flagship phones.

**Decoding input is the hard half, and the OS is the way through it.** Reading
the world's H.264/HEVC files without FFmpeg means one backend per platform:
VideoToolbox (macOS, iOS), Media Foundation / D3D11VA (Windows), MediaCodec
(Android), VA-API or V4L2-M2M (Linux). That is five backends — but they are
hardware-accelerated, they ship with the OS, and the patent licence is the OS
vendor's, not ours. Behind them sits a portable software path — dav1d/rav1d for
AV1, libvpx for VP9 — so vtome can always read *its own* format everywhere, even
where the OS offers nothing.

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
| VP9 encode/decode | `vpx-sys` (libvpx) | C, safe wrapper | Fallback both ways |

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
- [ ] **macOS/iOS** — VideoToolbox via the `objc2` family. H.264, HEVC, and AV1
      on hardware that has it; frames arrive as `CVPixelBuffer`, which maps to a
      Metal texture with no copy (see §5's zero-copy path)
- [ ] **Windows** — Media Foundation / D3D11VA through the `windows` crate.
      Output is a D3D11 texture; wgpu's DX12 backend needs it shared, so this is
      the interop that will take the longest
- [ ] **Android** — MediaCodec through `ndk`. Decode to a `SurfaceTexture` and
      sample it as an external texture; never read frames back to the CPU
- [ ] **Linux** — VA-API (`cros-libva`) with a V4L2-M2M path for ARM boards, and
      a documented "software only" outcome where neither exists
- [ ] **Portable software AV1** — dav1d via `dav1d-rs`, or `rav1d` if its
      release state holds up. This is the floor: it is what makes "vtome can
      always play what vtome wrote" true on every target
- [ ] **Portable software VP9** — libvpx via `vpx-sys`
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
- [ ] VP9 via libvpx, behind `encode-vp9`, for the devices where hardware AV1
      decode is not there yet
- [ ] Mux to WebM (`webm-iterable`) as the native container; AV1-in-MP4 as an
      export option, since that is what more players open
- [ ] Progress callback and cancellation. A 4K transcode is minutes to hours and
      must be interruptible — the same shape pfac wants for bundling
- [ ] Resolution and frame-rate change on the way through, done on the GPU when
      a GPU is present and in a small pure-Rust scaler when it is not
- [ ] Pass-through: an input already AV1 or VP9 is remuxed, not re-encoded.
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
- [ ] `Player` tying a demuxer, a decoder, and the clock together. Waiting on
      §2 — there is nothing to pace until something decodes
- [ ] Seek: to the keyframe from §1's index, then decode-and-discard to the
      exact frame
- [ ] Gapless transition between two files, and A/B crossfade, since this exists
      to feed a show-control application
- [ ] Multi-output sync: several monitors showing several videos that must start
      on the same frame

## 10. Testing

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

## 11. Decisions still open

- [ ] **H.264 input on Linux.** VA-API where the driver has it; otherwise
      `openh264` built from source, whose patent position is not Cisco's binary
      licence. Write the conclusion down in the README rather than leaving it
      implied
- [ ] **`rav1d` vs `dav1d-rs`.** The Rust port removes the C toolchain from every
      build, which is worth real effort — but only if its releases and its
      performance are there. Benchmark both before committing
- [ ] **HEVC** input, which is patent-encumbered to decode as well. Platform
      decoders sidestep it; a software fallback would not
- [ ] Whether vtome ever writes MP4 itself or only WebM
- [ ] HDR: tone-map to SDR at first, or carry PQ/HLG through to a display that
      can take it
- [ ] External displays on iOS and Android, which do exist and are nothing like
      a desktop monitor list

## 12. Later

- [ ] Hardware encode, if §4's measurement ever justifies it
- [ ] Network sources: HTTP range requests, HLS/DASH — vtome reading from a
      `Read + Seek` rather than only a `File`, the same shape pfac wants
- [ ] Playing straight out of a pfac bundle, since `pfac::Bundle::stream` is
      already `Read + Seek + Send` and that is exactly what a demuxer needs
- [ ] Capture: screen or camera in, as a frame source
- [ ] Real-time effects between decode and present, as shader passes
- [ ] Deinterlacing, for the archival footage that will inevitably turn up
