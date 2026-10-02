# vtome

**V**ideo **T**ranslucent **O**ptimized **M**acGyver **E**ngine — put an image or
a video on a specific monitor, or on a specific *quadrilateral* of one, without
FFmpeg and without a codec anyone charges for.

```rust
use vtome::window::Viewer;
use vtome::{MonitorSelector, Placement};

let frame = vtome::load_image("poster.png")?;

// The projector, keystoned because it is aimed upwards at the wall: the top
// edge inset a tenth of the width at each end. A fraction rather than pixels,
// so the same configuration works on a 1080p projector and a 4K one.
let placement = Placement::new(MonitorSelector::Name("EPSON".into())).keystone(0.1);

Viewer::new(frame, placement).show()?;
```

`Placement::corners` takes four arbitrary corners when a symmetric keystone is
not the shape you need.

A film goes up the same way, as an overlay — no window frame, transparent around
the picture, above everything else, and optionally letting the mouse through:

```rust
let source = vtome::VideoSource::from_file("clip.mp4")?;
let placement = Placement::new(MonitorSelector::Primary)
    .area(vtome::geometry::Rect::new(40.0, 40.0, 640.0, 360.0))
    .always_on_top(true);

let report = Viewer::video(source, placement).click_through(true).show()?;
println!("{} shown, {} dropped", report.presented, report.dropped);
```

## The trapezoid is the point

Putting a picture into four arbitrary corners is not a matter of moving the
vertices. A quadrilateral that is not a parallelogram needs a **projective** map,
and interpolating texture coordinates linearly across the two triangles of a quad
is wrong everywhere except the corners — a visible crease down the diagonal,
which is exactly what appears when a projector is off-axis to a wall.

vtome computes the homography from the unit square onto your corners, hands the
shader its **inverse**, and lets each pixel ask which texel belongs to it. The
divide by `w` happens per pixel, so there is no crease because there is no
diagonal. `cargo run --example corner_pin` prints the difference in pixels:

```text
the centre of the picture:
  projective   (  960.0,   462.9)  ← where it belongs
  averaged     (  960.0,   540.0)  ← what linear UV interpolation gives
  they differ by 77.1 px. That difference is the crease down the diagonal.
```

A quad that folds over itself is `Error::Placement` rather than something drawn
inside out.

## Royalty-free is a constraint, not a preference

vtome decodes **H.264** and **AV1**, and writes **AV1** — nothing else. AV1 is
AOMedia royalty-free, and `rav1e` encodes it in pure Rust. H.264 is never
written, and it is only ever *read* through the decoder the operating system
already ships and already licensed: VideoToolbox, Media Foundation, MediaCodec,
VA-API. That is also the fast path, since those are the hardware decoders.
`Encoding::is_encodable()` is the same rule in code, and a test asserts it from
outside the crate so it cannot quietly change.

## What works today

| | |
| --- | --- |
| `identify` | Container from magic bytes, never the extension — MP4/MOV, Matroska/WebM, AVIF/HEIF, PNG, JPEG, GIF, WebP, BMP, TIFF |
| `demux` | MP4 and Matroska/WebM taken apart: tracks, timing, keyframes, colour metadata, parameter sets. Pure Rust |
| `still` | Images in, as frames, down the same pipe as video |
| `geometry` | Rectangles, convex quads, homographies, and the fit modes |
| `placement` | Monitor selectors, areas, corner pinning, late resolution with a stated fallback |
| `decode` | H.264 through VideoToolbox on macOS and iOS: hardware decode, NV12 straight to the shader, B-frames put back in display order |
| `render` | wgpu: YUV→RGB and corner pinning in one shader pass, offscreen or onto a surface |
| `window` | winit: a still or a film as an overlay — undecorated, transparent, optionally on top and click-through — on the monitor you named |
| `clock` | Playback timing, and slaving video to an external (audio) master |
| `bitstream` | Annex-B ↔ length-prefixed, and `avcC` parameter sets |

**Not yet:** H.264 decoders on Windows, Android, and Linux; AV1 decoding (dav1d,
and VideoToolbox on M3-class hardware); encoding and transcoding. Each is planned
in `planning/TODO.md` §2 and §4. Where there is no decoder, opening a file says
which feature or platform would have handled it — deliberately, rather than a
decoder that returns no frames and a black window.

So today vtome shows **still images** anywhere on your desktop, in any convex
quadrilateral, and on a Mac **plays H.264** there too.

## Everything heavy is optional

```toml
[dependencies]
vtome = { version = "0.1", default-features = false, features = ["demux"] }
```

| Feature | What it adds | Default |
| --- | --- | --- |
| `demux` | MP4 + Matroska/WebM parsing | ✓ |
| `image` | Still images | ✓ |
| `render` | wgpu. A GPU and a surface — *not* a window | |
| `window` | `render` + winit: vtome opens its own windows | |
| `embed` | `render` against a surface someone else owns — the Tauri path | |
| `decode-platform` | The OS's own decoder — VideoToolbox so far. Links system frameworks; pulls in no crates | |
| `decode-av1` | AV1 in software, everywhere (not implemented yet) | |
| `encode-av1`, `mux`, `transcode` | Writing AV1 into WebM | |

A build that decodes frames and hands them to somebody else's renderer compiles
no windowing library, no GPU abstraction, and no C toolchain.

## Embedding in Tauri, or any host that owns its window

Take `render` (and `embed`) rather than `window`, and give vtome a surface:

```rust
let gpu = vtome::render::Gpu::from_instance(instance, Some(&surface))?;
let mut renderer = vtome::render::Renderer::new(&gpu, surface_format)?;

renderer.upload(&gpu, &frame)?;
renderer.draw(&gpu, &view, width, height, quad, 1.0)?;
```

Do **not** ship decoded frames over the IPC bridge to a canvas: a 4K frame is
about 12 MB, and 24 fps of them is ~300 MB/s through a JSON channel. Render into
a native surface composited with the webview instead.

## Audio is somebody else's job

vtome never opens an audio device. Its demuxers report that audio tracks exist
and hand their packets over untouched; pair it with [`atome`](../atome) and slave
video to the audio clock:

```rust
impl vtome::MasterClock for MyAudioEngine {
    fn position(&self) -> std::time::Duration { self.play_position() }
}
```

Audio leads, always. A dropped frame is invisible; a stuttered audio buffer is
not.

## Building and testing

```sh
make test          # everything, including the GPU tests where there is a GPU
make corner-pin    # the homography, printed
make monitors      # what is attached
make show FILE=poster.png MONITOR=1 KEYSTONE=0.15
make play FILE=clip.mp4 AREA=40,40,640,360 CLICK_THROUGH=1 ONCE=1
make contact-sheet FILE=clip.mp4   # decode it all, save six frames as a PNG
```

### On every operating system

```sh
make docker-test                           # every system this host can run
make docker-test SYSTEMS="debian alpine"   # just these
```

Runs from Linux, Windows (`docker\run.ps1`, or `run.sh` from Git Bash), or an
Apple-silicon Mac. Debian, Ubuntu, Fedora, and Alpine run the tests in
containers — GPU tests included, on Mesa's software Vulkan — each carrying
ffmpeg to rebuild the fixtures. Windows is cross-built and tested under Wine;
Android is cross-built with the NDK. The native systems are skipped wherever
they cannot run rather than failed: macOS (and the iOS cross-build) runs on a
Mac, and a real Windows container only on a Windows host.

The decoder's tests play `tests/data/bars_h264.mp4` — 2.7 KB, 24 frames, with
B-frames — whose moving bar says which frame each picture is, so display order
is checked from the pixels rather than trusted from timestamps. vtome cannot
write H.264, so that fixture is committed; `tests/data/make_h264_fixture.sh`
rebuilds it.

The renderer's tests draw on a real GPU and read the pixels back — a keystoned
quad has to come out narrower at the top, and the picture's midline has to land
within three pixels of where the homography says. Where there is no adapter they
skip with a message rather than failing.

## License

Licensed under the [MIT License](LICENSE).
