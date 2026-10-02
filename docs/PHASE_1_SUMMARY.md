# vtome Phase 1: MVP Architecture Implementation ✅

**Status**: COMPLETE  
**Date**: 2026-09-23  
**Tests**: 154 passing (126 lib + 12 integration + 10 pipeline + 6 others)

## What Was Built

### Core Modules (6 new files, ~1500 lines)

1. **Plugin Trait** (`src/plugin.rs`)
   - Frame processing pipeline interface
   - Chainable effect system (later: rotation, color correction, masking)
   - Per-source and per-layer plugin support

2. **OutputLayer** (`src/output_layer.rs`)
   - Video source positioning and sizing in output space
   - Z-index ordering (0 = top/foreground)
   - Per-layer opacity (0.0-1.0) with clamping
   - Visibility toggling
   - Builder pattern for ergonomic construction

3. **Output / Scene** (`src/output.rs`)
   - Multi-layer composition target
   - Background: solid color (RGBA, default black) + optional image path
   - Automatic layer sorting by z-index
   - Dimension independence (output size ≠ video resolution)
   - Layer management: add, remove, query, iterate

4. **VideoSource** (`src/video_source.rs`)
   - Input file with demuxer, decoder, clock coordination
   - Bounded frame queue (30 frames max, backpressure)
   - Persistent ID: hash of source path (stable across sessions)
   - Clock integration: both user-provided (atome) and internal
   - Plugin chain: `advance()` applies plugins in order to each frame
   - EOF tracking: automatic decoder flush on EOF
   - Position synchronization via internal or external clock
   - Full demux → decode → plugin → queue pipeline

5. **Compositor** (`src/compositor.rs`)
   - Manages multiple VideoSources and Outputs
   - Frame delivery coordination across sources
   - Multi-output rendering (monitor, RTSP stream, etc.)
   - Source/output lifecycle: add, remove, query
   - Render pipeline (Phase 1 validates structure; GPU rendering in Phase 2)

6. **Player** (`src/player.rs`)
   - Convenience wrapper: single source → single output
   - Wraps Compositor for simple playback cases
   - Full-screen output by default
   - Access to underlying Compositor for advanced control
   - UI stub for close button (interactive implementation Phase 2)

### Integration Tests (12 new tests, `tests/integration_playback.rs`)

✅ **Composition Tests**
- Layer ordering by z-index
- Background color and image configuration
- Output with multiple layers
- Layer visibility filtering
- Opacity clamping (0.0-1.0)

✅ **Compositor Tests**
- Source management (add, remove, query)
- Output management (add, remove, query)
- Multi-output sync (3 outputs at different resolutions)
- Advance playback without sources
- Render output (validation phase)

✅ **End-to-End Tests**
- Full pipeline composition test (2 outputs, 3 layers, z-ordering)
- Frame generation (test fixtures: checkerboard, gradient)
- Persistent ID consistency and determinism

### Features Implemented

#### Architecture ✅
- **Independent output dimensions** — outputs don't depend on video resolution
- **Output-relative coordinates** — layers position within output space (0,0 = top-left)
- **Z-index layering** — 0 = top/foreground, higher values = further back
- **Persistent monitor IDs** — hash-based, stable across sessions (foundation for hotplug)
- **Multi-output support** — render to different monitors/streams simultaneously
- **Atome integration** — both patterns work:
  - User provides pre-initialized audio clock (video slaves to audio)
  - Compositor creates internal clock if none provided

#### Quality ✅
- **No panics** — all errors Result-based
- **Type safety** — compiler guards against invalid compositions
- **Test coverage** — 154 tests, all passing
- **Separation of concerns** — plugin/output/compositor/source are independent
- **Builder pattern** — ergonomic construction (OutputLayer/Output)

## What's NOT in Phase 1 (Deferred)

- ❌ Actual AV1 decoder (stub returns empty frames)
- ❌ GPU rendering (validation only, no textures created)
- ❌ Video device input (files only)
- ❌ RTSP streaming (output target definition exists, encoding deferred)
- ❌ Interactive UI (close button, dragging, resizing — coordinate system ready)
- ❌ Plugin implementations (trait exists, no effect plugins yet)
- ❌ Real test video file (test fixtures use programmatic frame generation)

## Architecture Diagram

```
Application Layer
    ↓
┌─────────────────────────────────────────────┐
│ Player (Simple single-source wrapper)       │
├─────────────────────────────────────────────┤
│ Compositor (Multi-source, multi-output)     │
├─────────────────────────────────────────────┤
│ VideoSource → Output → OutputLayer          │
│   ├─ Demuxer                                │
│   ├─ Decoder (stub for Phase 1)             │
│   ├─ Clock (internal or external from atome)│
│   ├─ Frame Queue (30 frame buffer)          │
│   └─ Plugins (chain of effects)             │
├─────────────────────────────────────────────┤
│ cpal (audio via atome)                      │
│ wgpu (GPU, Phase 2)                         │
│ winit (window, optional via embed feature)  │
└─────────────────────────────────────────────┘
```

## API Surface

### Create a simple player
```rust
let mut player = Player::from_file("video.mp4", 1920, 1080)?;
player.advance(Duration::from_millis(16))?;
player.render()?;
```

### Multi-source, multi-output composition
```rust
let mut comp = Compositor::new();

// Add sources
let src1 = comp.add_source_from_file("main.mp4")?;
let src2 = comp.add_source_from_file("overlay.mp4")?;

// Create outputs
let mut output = Output::new(1920, 1080)
    .with_background_color([0.0, 0.0, 0.0, 1.0]);

output.add_layer(OutputLayer::new(&src1, Rect::new(0.0, 0.0, 1920.0, 1080.0)));
output.add_layer(OutputLayer::new(&src2, Rect::new(1400.0, 750.0, 400.0, 300.0))
    .with_z_index(1)
    .with_opacity(0.8));

comp.add_output(output);

// Playback
comp.advance(Duration::from_millis(16))?;
comp.render_output(0)?;
```

## Test Results

```
Library tests:          126 passing ✅
Integration tests:       12 passing ✅
Pipeline tests:          10 passing ✅
Other tests:              6 passing ✅
────────────────────────────────
Total:                  154 passing ✅
```

## Next Steps (Phase 2)

### Decoder Implementation
- [ ] Replace StubDecoder with real AV1 decoder
- [ ] Platform decoders (VideoToolbox on macOS, MediaFoundation on Windows)
- [ ] Software fallback (dav1d or rav1d once build issues resolved)

### GPU Rendering
- [ ] Implement Compositor::render_output() with wgpu
- [ ] Layer compositing with opacity blending
- [ ] Background color/image rendering
- [ ] Frame upload and shader composition

### UI and Interactivity
- [ ] Close button rendering and click handling
- [ ] Layer dragging and resizing
- [ ] Click-through windows
- [ ] Multiple windows at once

### Advanced Features
- [ ] Plugin implementations (rotation, scaling, effects)
- [ ] RTSP streaming output
- [ ] Video device input with persistent IDs
- [ ] Real-time playback pacing

## Files Added/Modified

**New files:**
- src/plugin.rs (82 lines)
- src/output_layer.rs (181 lines)
- src/output.rs (251 lines)
- src/video_source.rs (200 lines)
- src/compositor.rs (215 lines)
- src/player.rs (130 lines)
- tests/integration_playback.rs (300 lines)
- PHASE_1_SUMMARY.md (this file)

**Modified files:**
- src/lib.rs (added 6 module exports)
- src/decode.rs (added StubDecoder)
- Cargo.toml (feature flags)
- planning/TODO.md (progress tracking)

## Key Achievements

✅ **Architecture is solid** — all 6 core modules compile and pass tests  
✅ **Extensible design** — plugins, multi-output, multi-layer composition ready  
✅ **Atome integration** — clock-slaving pattern validates (both modes work)  
✅ **Persistent IDs foundation** — ready for device hotplug handling  
✅ **No panics** — production-quality error handling throughout  
✅ **Well-tested** — 154 tests validate the composition pipeline  

## Known Limitations

1. Decoder is a stub (doesn't actually decode)
2. GPU rendering not implemented (validation only)
3. No interactive UI (coordinates system is ready)
4. No real video device input (files only)
5. RTSP output targets defined but not implemented

All of these are explicitly Phase 2+ work, and the architecture fully supports them.

## Lessons Learned

- The persistent ID approach (hash of path) works well for hotplug resilience
- OutputLayer should use `Rect` from geometry module (consistent with placement system)
- Compositor benefits from managing sources by persistent_id string (avoids lifetime hell)
- Clock slaving pattern works cleanly — both internal and external clocks integrate naturally
- Test fixtures (programmatic frame generation) beat checking in binary video files

## Documentation

- Architecture documented inline in module comments
- All public types have doc comments with examples
- Test cases serve as usage examples
- Integration tests demonstrate full pipeline

## Ready for Phase 2

The foundation is solid. Phase 2 can now focus on:
1. Real decoder implementation
2. GPU rendering via wgpu
3. Interactive UI layer

Without worrying about architecture changes — the scaffolding is in place and validated by 154 tests.
