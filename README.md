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

vtome **writes** AV1 and VP9 — both AOMedia/Google royalty-free. It never writes
H.264 or HEVC. It **reads** them through the decoder the operating system already
ships and already licensed: VideoToolbox, Media Foundation, MediaCodec, VA-API.
That is also the fast path, since those are the hardware decoders.

AV1 first, because `rav1e` encodes it in pure Rust — where VP9 means libvpx and a
C toolchain on five platforms. `Encoding::is_encodable()` is the same rule in
code, and a test asserts it from outside the crate so it cannot quietly change.

## What works today

| | |
| --- | --- |
| `identify` | Container from magic bytes, never the extension — MP4/MOV, Matroska/WebM, AVIF/HEIF, PNG, JPEG, GIF, WebP, BMP, TIFF |
| `demux` | MP4 and Matroska/WebM taken apart: tracks, timing, keyframes, colour metadata, parameter sets. Pure Rust |
| `still` | Images in, as frames, down the same pipe as video |
| `geometry` | Rectangles, convex quads, homographies, and the fit modes |
| `placement` | Monitor selectors, areas, corner pinning, late resolution with a stated fallback |
| `render` | wgpu: YUV→RGB and corner pinning in one shader pass, offscreen or onto a surface |
| `window` | winit: an undecorated, transparent window on the monitor you named |
| `clock` | Playback timing, and slaving video to an external (audio) master |
| `bitstream` | Annex-B ↔ length-prefixed, and `avcC` parameter sets |

**Not yet:** the decoders themselves. `decode` is the trait, the backend
selection, and an error that names the backend that would have taken the work —
deliberately, rather than a decoder that returns no frames and a black window.
Encoding and transcoding are the same: planned in `planning/TODO.md` §2 and §4,
not pretended at.

So today vtome shows **still images** anywhere on your desktop, in any convex
quadrilateral, and knows everything about a video file except how to turn its
packets into pictures.

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
| `decode-av1`, `decode-vp9`, `decode-platform` | One decoder backend each | |
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
```

The renderer's tests draw on a real GPU and read the pixels back — a keystoned
quad has to come out narrower at the top, and the picture's midline has to land
within three pixels of where the homography says. Where there is no adapter they
skip with a message rather than failing.

## License

Licensed under the [MIT License](LICENSE).
