# vtome

**V**ideo **T**ranslucent **O**ptimized **M**acGyver **E**ngine — put an image or
a video on a specific monitor, or on a specific *quadrilateral* of one, without
FFmpeg and without bundling a codec anyone charges for.

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

## H.264 by default, and only ever the operating system's

vtome plays and writes **H.264** — it hardware-decodes on every machine a file
is likely to reach — and H.264 is patent-pooled, so vtome never bundles an H.264
encoder or decoder. Every H.264 frame in or out goes through the codec the
operating system ships and already licensed:

| Platform | H.264 decode and encode |
| --- | --- |
| macOS, iOS | VideoToolbox |
| Windows | Media Foundation |
| Android | MediaCodec |
| Linux | none yet (VA-API is planned) — refused by name, never bundled |

The same rule holds for the audio half: AAC is decoded by
[`atome`](../atome) through AudioToolbox, Media Foundation, or MediaCodec — never
Symphonia's AAC decoder or libfdk-aac, neither of which is compiled into either
crate. `cargo tree -i` finds no AAC or H.264 codec crate in either dependency
graph with every feature on.

A transcode can write to a path (`transcode`) or to any
`Read + Write + Seek + Send` (`transcode_into`) — a file already open, or an
entry of a [`pfac`](https://crates.io/crates/pfac) bundle, so the result lands
in the project it belongs to without being written somewhere else first. MP4's
index is moved to the front inside the destination itself.

**AV1** is royalty-free, so its encoder, `rav1e`, is bundled with every transcode
build — but written only when asked for: vtome cannot yet play AV1 back on most
machines (dav1d is planned), and the AV1 decoder refuses by name rather than
opening a file and showing nothing. HEVC and VP9 are out of scope both ways.

## Import: a playable file now, the real one later

```rust
let job = vtome::import("camera/take-3.mov", "show/take-3.mp4", Some("show/take-3.flac"))?;

// Poll from a UI, say every 500 ms:
for file in vtome::import_progress() {
    println!("{}: {:?} {:.0}%", file.output.display(), file.stage, file.fraction * 100.0);
}
```

`import` checks the file at the call — a container vtome reads, a video track,
something here to decode it and encode it — and returns at once. A small proxy
(640 px at most, the encoder's fastest settings) is written to the output path
first, so the path plays straight away; the full-quality file encodes in a temp
directory and replaces the proxy in one rename. Jobs go through a queue — one
lane for proxies and audio, one for final encodes — so a hundred imports never
mean a hundred encoders. `cancel`, `wait`, and `ImportOptions::hardware`
(`Prefer`, `Require`, `Off`) are on `vtome::import_queue()`.

Every H.264 file it writes meets one spec, chosen so any hardware decoder takes
it instantly: High profile with **Level 4.1** declared, 8-bit 4:2:0, no
B-frames, a keyframe every two seconds exactly (closed GOPs), CABAC, and `moov`
before `mdat`. A picture bigger or faster than 4.1 allows — 4K, 1080p60 — is
scaled to fit. The tests check each of those from the files' own bytes, and with
`ffprobe` where it is installed.

## The engine: outputs per monitor, clips from any thread, sound in step

```rust
use std::collections::HashMap;
use vtome::{Audio, Clip, Vtome};

let output = atome::output::OutputClass::<f32>::new(/* … */);
let mut vtome = Vtome::new(["monitor_e16def3726bc150b"], 30.0);
let controls = vtome.controls();

std::thread::spawn(move || {
    // The film's own soundtrack plays through atome, on the same clock as the
    // pictures. A film without one can be given a file — the FLAC `import`
    // split out, say.
    let take = controls.add(Clip::video("show/take-3.mp4", "monitor_342b…").audio("show/take-3.flac"))?;
    let logo = controls.add_generic("logo.png", "monitor_342b…")?; // still or video, by content
    controls.stop(logo);
    assert!(controls.is_finished(logo));
    let frame = controls.snapshot(take)?;
    # Ok::<(), vtome::Error>(())
});

let outputs = HashMap::from([("monitor_342b…".to_string(), (1920, 1080, 0, 0))]);
vtome.start(outputs, HashMap::new(), Audio::atome(&output))?;
```

`TauriVtome` is the same engine inside a Tauri application, which owns the event
loop; its `start` returns the attached monitors. `add_cached` plays ranges of a
film from frames already decoded (`cache_frames` makes them) and decodes only
between them. `Clip::start_at` puts a film's first frame at an exact point on the
engine's clock — atome's output clock, with audio — which is how a picture lines
up with a sound scheduled there.

Audio leads, always. Every clip follows one timeline, and with `Audio::atome`
that timeline is what the speakers are playing, latency included. Stopping a
clip takes its soundtrack back within a buffer. A dropped frame is invisible; a
stuttered audio buffer is not.

## What works today

| | |
| --- | --- |
| `identify` | Container from magic bytes, never the extension — MP4/MOV, Matroska/WebM, AVIF/HEIF, PNG, JPEG, GIF, WebP, BMP, TIFF |
| `demux` | MP4 and Matroska/WebM taken apart: tracks, timing, keyframes, colour (from the SPS, not guessed), parameter sets. Pure Rust |
| `decode` | H.264 through the OS: VideoToolbox, Media Foundation, MediaCodec. Hardware, software, or either |
| `encode`, `mux`, `transcode`, `import` | H.264 through the OS into faststart MP4; AV1 through rav1e into WebM; the queue above |
| `scale` | NV12 ↔ I420 and area-averaged shrinking, in pure Rust |
| `still` | Images in, as frames, down the same pipe as video |
| `geometry`, `placement` | Rectangles, convex quads, homographies, fit modes; monitor selectors resolved late with a stated fallback |
| `render` | wgpu: YUV→RGB and corner pinning in one shader pass, offscreen or onto a surface |
| `window`, `tauri` | The engine in vtome's own windows, or a Tauri application's |
| `clock` | One timeline for every clip, following atome's output clock or a monotonic one |
| `bitstream` | Annex-B ↔ length-prefixed, `avcC` records, SPS parsing |

Windows and Android are compiled and type-checked on every change; they have
not yet been run on those systems' hardware.

## Everything heavy is optional

```toml
[dependencies]
vtome = { version = "0.2", default-features = false, features = ["demux"] }
```

| Feature | What it adds | Default |
| --- | --- | --- |
| `demux` | MP4 + Matroska/WebM parsing | ✓ |
| `image` | Still images | ✓ |
| `decode-platform` | The OS's H.264 decoder. System frameworks on Apple and Android; the `windows` crate on Windows | |
| `encode-platform` | The OS's H.264 encoder, likewise | |
| `encode-av1` | rav1e, bundled | |
| `mux` | MP4 and WebM writing | |
| `transcode` (= `import`) | `transcode` and `import`; brings the three above | |
| `atome` | Video on atome's clock, and soundtracks through atome | |
| `split-audio` | `import`'s audio split to FLAC, through atome | |
| `render` | wgpu. A GPU and a surface — *not* a window | |
| `window` | `render` + winit: vtome opens its own windows | |
| `tauri` | The engine inside a Tauri application | |
| `embed` | `render` against a surface someone else owns | |
| `decode-av1` | AV1 in software (refuses until dav1d is wired up) | |

## Embedding in Tauri, or any host that owns its window

Take `tauri` for the whole engine, or `render` (and `embed`) to draw into a
surface you own:

```rust
let gpu = vtome::render::Gpu::from_instance(instance, Some(&surface))?;
let mut renderer = vtome::render::Renderer::new(&gpu, surface_format)?;

renderer.upload(&gpu, &frame)?;
renderer.draw(&gpu, &view, width, height, quad, 1.0)?;
```

Do **not** ship decoded frames over the IPC bridge to a canvas: a 4K frame is
about 12 MB, and 24 fps of them is ~300 MB/s through a JSON channel. Render into
a native surface composited with the webview instead.

## Building and testing

```sh
make test          # the whole suite: GPU, VideoToolbox, transcode, import, audio
make corner-pin    # the homography, printed
make monitors      # what is attached
make show FILE=poster.png MONITOR=1 KEYSTONE=0.15
make play FILE=clip.mp4 AREA=40,40,640,360 CLICK_THROUGH=1 ONCE=1
make contact-sheet FILE=clip.mp4   # decode it all, save six frames as a PNG
```

The decoder's tests play `tests/data/bars_h264.mp4` — 2.7 KB, 24 frames, with
B-frames — whose moving bar says which frame each picture is, so display order,
seeking, and frame caches are checked from the pixels rather than trusted from
timestamps. Files vtome writes are decoded again and checked the same way. The
renderer's tests draw on a real GPU and read the pixels back. Tests that need a
GPU, an audio device, or `ffmpeg` (to make a fixture with sound) skip with a
message where there is none.

### On every operating system — for special occasions

```sh
make docker-test                           # every system this host can run
make docker-test SYSTEMS="debian alpine"   # just these
```

Not part of `make test`, and never run with it. Debian, Ubuntu, Fedora, and
Alpine run the tests in containers; Windows is cross-built and tested under
Wine; Android is cross-built with the NDK; macOS (and the iOS cross-build) runs
natively on a Mac, into a target directory of its own.

## License

Licensed under the [MIT License](LICENSE).
