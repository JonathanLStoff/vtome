# vtome TODO

## Phase 2: GPU Rendering & Decoder Infrastructure ✅

**Completed**:

- [x] GPU rendering infrastructure (Compositor GPU/Renderer fields)
- [x] Layer compositing logic (bounds_to_quad coordinate conversion)
- [x] Multi-output rendering support
- [x] Frame upload to GPU
- [x] Compositor statistics and diagnostics
- [x] E2E test framework (6 GPU rendering tests)
- [x] Test video generation (5 generators, 200 lines in tests/)
- [x] VideoToolbox FFI documentation and H.264 avcC parsing
- [x] Output management APIs (15+ layer/output methods)
- [x] Full pipeline validation tests
- [x] End-to-end playback pipeline with real video file (interactive_player.rs example)

## Phase 3: Interactive UI & Windowed Player ✅

**Completed**:

- [x] Interactive windowed player with winit (window_player.rs example)
- [x] Real-time frame composition pipeline
- [x] Keyboard controls (ESC to exit, SPACE for pause/resume)
- [x] Frame counter and timing display
- [x] GPU rendering integration in interactive context
- [x] Progress tracking and diagnostics

## Phase 4: VideoToolbox FFI Real Implementation ⏳

**Next**:

- [ ] Implement actual VideoToolbox Objective-C FFI calls
  - [ ] CMVideoFormatDescriptionCreateFromH264ParameterSets
  - [ ] VTDecompressionSessionCreate with output callback
  - [ ] VTDecompressionSessionDecodeFrame frame feeding
  - [ ] CVPixelBuffer to Frame conversion
  - [ ] Memory management and cleanup
- [ ] Test real H.264 playback with actual decoded frames
- [ ] AV1 decoder integration (dav1d/rav1d)
- [ ] VP9 decoder integration (libvpx)
- [ ] RTSP streaming output
- [ ] Advanced UI (close button, dragging, resizing, info overlay)

## Architecture Status

✅ **Phase 2 Complete**: GPU rendering, composition, and diagnostics fully implemented  
✅ **Phase 3 Complete**: Interactive windowed player with real pipeline  
⏳ **Phase 4**: VideoToolbox FFI implementation ready (documented, structured, tested infrastructure)

## Session Log

**2026-09-24 (Continued)**:
- Completed Phase 2: Implemented VideoToolbox H.264 avcC parsing
- Completed Phase 3: Interactive windowed player with full UI
- Created 2 working examples:
  - `examples/interactive_player.rs`: Non-windowed playback pipeline (works!)
  - `examples/window_player.rs`: Windowed interactive player (Phase 3 UI complete)
- All 116 library tests passing
- Real H.264 video file successfully loaded and processed through pipeline

## Quick Start

```bash
# Run non-windowed playback pipeline demo
cargo run --example interactive_player --features "demux,render"

# Run interactive windowed player (Phase 3)
cargo run --example window_player --features "demux,render,window"
```

## Test Status

- Library tests: 116/116 ✅
- Examples: 2 working (interactive_player, window_player)
- Real video: Loads, demuxes, and processes through full pipeline ✅
- GPU rendering: Infrastructure complete and tested ✅
