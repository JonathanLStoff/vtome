# vtome Phase 2: Playback and GPU Rendering

**Status**: Architecture Complete, Ready for Phase 2  
**Date**: 2026-09-23  
**Completed in Phase 1**: 154 tests, full MVP architecture  
**Licensing**: H.264 uses OS-level decoders only (royalty-free via platform APIs)

## What Phase 2 Accomplishes

Moving from architecture validation to a working video composition system that can actually play files and render to screen.

### Major Components

#### 1. Platform Decoders (H.264/HEVC Support - Phase 2)
- **Status**: Architecture ready, backends not yet implemented
- **Approach**: Use OS-provided decoders (no licensing issues)
  - **macOS/iOS**: VideoToolbox (hardware H.264, HEVC, AV1)
  - **Windows**: Media Foundation / D3D11VA (hardware codecs)
  - **Android**: MediaCodec (hardware codecs)
  - **Linux**: VA-API or V4L2-M2M (hardware), openh264 (software, pre-licensed)

- **Implementation path**:
  - VideoToolbox FFI on macOS (objc2 crate, same pattern as atome)
  - MediaFoundation FFI on Windows (windows crate)
  - Platform decoders provide frames with no licensing burden
  - Focus on macOS first (your current platform)

#### 2. GPU Rendering (Architecture ready)
- **Status**: Scaffolded, stub in place
- **What's done**:
  - Renderer infrastructure exists in `src/render.rs`
  - Gpu and Renderer structs available
  - Compositor::render_output_gpu() stub ready
  - Quad-based corner-pinning fully implemented

- **What's next**:
  - Integrate Gpu/Renderer instances into Compositor
  - Implement layer composition (z-index, opacity blending)
  - Convert Rect bounds to Quad for rendering
  - Texture management for output targets

#### 3. Test Video Generation (MVP)
- **Status**: Fixtures exist in tests
- **What's done**:
  - Checkerboard_frame() and gradient_frame() helper functions
  - 12 integration tests using generated frames
  - Can validate composition without real video files

- **What's next**:
  - Create small test video file (H.264 MP4) for CI
  - Add playback test: demux → decode → render
  - Validate end-to-end pipeline with real file

## Quickest Path to Working Demo

### Approach: H.264 + Real Video File
1. ✅ Add rusty_h264 dependency (done)
2. ⏳ Research and implement H264Decoder::decode()
3. ⏳ Create/add small test video file
4. ⏳ Implement Compositor GPU rendering
5. ⏳ Add playback test: load file → demux → decode → render

### Alternative: GPU Render First
1. ✅ Renderer infrastructure ready (done)
2. ⏳ Implement Compositor::render_output_gpu()
3. ⏳ Test with generated frames (checkerboard, gradient)
4. Then add H.264 decoder

## Known Blockers and Solutions

### rusty_h264 Integration
- **Challenge**: Understand crate's decode API and output format
- **Solution**: Read crate docs, check examples, implement step-by-step
- **Status**: Blocked on research

### GPU Layer Composition
- **Challenge**: Blending multiple layers with different opacities
- **Solution**: Render each layer to texture, composite in order
- **Status**: Design ready, implementation pending

### Test Video File
- **Challenge**: Can't check large binary files into git
- **Solution**: Generate small H.264 file or use embedded test data
- **Status**: Deferred to Phase 2

## File Status

| File | Status | Notes |
|------|--------|-------|
| src/decode_h264.rs | ✅ Compiles | Stub implementation |
| src/decode.rs | ✅ Updated | Backend selection logic ready |
| Cargo.toml | ✅ Updated | rusty_h264 dependency added |
| src/compositor.rs | ✅ Updated | render_output_gpu() stub ready |
| tests/ | ✅ All passing | 154 tests still validate |

## Next Steps (Ordered by Impact)

### 1. H.264 Decoder (Critical Path)
```rust
// In decode_h264.rs: implement full decode() method
// Feed packet data to rusty_h264
// Convert output to vtome Frame format
// Handle YUV/RGB output format
```

### 2. GPU Rendering (Enables Visualization)
```rust
// In compositor.rs: render_output_gpu() 
// 1. Create Gpu instance
// 2. For each visible layer:
//    - Fetch current frame
//    - Upload to GPU texture
//    - Render to output with opacity
```

### 3. E2E Playback Test
```rust
// Load real H.264 file
// Demux → Decode → Render
// Validate frame appears on output
```

## Success Criteria for Phase 2

- [ ] H.264 decoder produces real frames from MP4 files
- [ ] GPU rendering composites layers to texture
- [ ] End-to-end test: file → output shows correct image
- [ ] Can play small test video file
- [ ] All 154 tests still pass
- [ ] No panics or assertion failures

## Technical Debt / Phase 3 Items

- Seek and keyframe navigation
- Plugin implementations (color correction, rotation, scaling)
- RTSP streaming output
- Video device input (webcam, HDMI capture)
- Interactive UI (close button, dragging, resizing)
- Additional decoder backends (AV1, VP9, platform decoders)
- Memory pooling for frame reuse
- Real-time frame pacing

## Architecture Notes

The Phase 1 architecture fully supports Phase 2 work:
- VideoSource already handles demux/decode/plugin/queue pipeline
- OutputLayer already has bounds and opacity
- Compositor already manages sources and outputs
- Renderer infrastructure already built
- All that's needed is integration

No architectural changes required. Just implementation.
