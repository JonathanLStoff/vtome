# vtome TODO

The plan lives in [`planning/TODO.md`](planning/TODO.md); finished work moves to
[`CHANGELOG.md`](CHANGELOG.md). This file is the short version.

**Scope:** decode H.264 (OS decoders only) and AV1; write AV1. No HEVC, no VP9.

## In progress

- [ ] Nothing — the VideoToolbox H.264 work is done and in the CHANGELOG

## Next

- [ ] The `Vtome` engine OrbitX drives (planning §13): `new(excluded_monitors,
      frame_rate)`; `start(outputs, backgrounds)` putting an image or colour on
      every listed monitor; `add(video or image)` → id, and `stop(id)`, with a
      video's audio handed to atome
- [ ] `import(input, output)` → AV1 on disk, optionally with the audio split
      out as FLAC for atome (planning §13, and §4)
- [ ] Two versions of `Vtome` with one API: standalone (vtome's own winit loop
      and windows) and Tauri (new borderless Tauri windows via OrbitX's
      `AppHandle`), since Tauri owns the event loop (planning §13)
- [ ] The compositor draws layers: per-layer textures, draw on top instead of
      clearing, quads in pixels, background colour or image first (planning
      §10.6)
- [ ] AAC through the OS decoder, never a bundled one — through atome
      (planning §13, atome planning §3.3)
- [ ] AV1 decode: dav1d/rav1d everywhere, VideoToolbox on M3-class hardware
      (`decode_av1.rs` is a stub that returns no frames — it should refuse
      until it decodes)
- [ ] AV1 encode through `rav1e`, and the transcode path (planning §4)
- [ ] H.264 decode on Windows, Android, and Linux (planning §2)
- [ ] Remove the VP9 scaffolding: `decode-vp9`, `encode-vp9`, `decode_vp9.rs`,
      `Backend::LibVpx`
- [ ] Zero-copy: the `CVPixelBuffer`'s IOSurface as a Metal texture
- [ ] Decode off the render thread
- [ ] `Compositor::render_output_gpu` uploads frames but never draws them; the
      overlay path (`Viewer::video`) does not use it

## Correction

An earlier version of this file said phases 2 and 3 were complete and that a
real H.264 file played end to end. It did not: `VideoSource` was silently
substituting a decoder that produced no frames, so the "152 frames rendered"
demo decoded nothing. Real decoding landed on 2026-09-24 and is tested in
`tests/videotoolbox.rs`.

## Try it

```sh
make play FILE=clip.mp4 AREA=40,40,640,360 CLICK_THROUGH=1 ONCE=1
make contact-sheet FILE=clip.mp4
make test
```
