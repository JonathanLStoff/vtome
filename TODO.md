# vtome TODO

## Phase 2: GPU Rendering & Decoder Infrastructure

### ✅ Completed

- [x] GPU rendering infrastructure (Compositor GPU/Renderer fields)
- [x] Layer compositing logic (bounds_to_quad coordinate conversion)
- [x] Multi-output rendering support
- [x] Frame upload to GPU
- [x] Compositor statistics and diagnostics
- [x] E2E test framework (6 GPU rendering tests)
- [x] Test video generation (5 generators, 200 lines in tests/)
- [x] VideoToolbox FFI documentation (comprehensive code comments in decode_videotoolbox.rs)
- [x] Output management APIs (15+ layer/output methods)
- [x] Full pipeline validation tests

### 🔄 In Progress

- [ ] VideoToolbox H.264 FFI implementation (CMVideoFormatDescription, VTDecompressionSession, CVPixelBuffer conversion)
  - [ ] Parse avcC box for SPS/PPS
  - [ ] Create CMVideoFormatDescription from parameters
  - [ ] Setup VTDecompressionSession with callback
  - [ ] Implement output callback to queue decoded frames
  - [ ] Convert CVPixelBuffer to Frame format
  
### ⏳ Next

- [ ] Real H.264 playback testing with actual video file
- [ ] AV1 decoder integration (dav1d/rav1d)
- [ ] VP9 decoder integration (libvpx)
- [ ] RTSP streaming output
- [ ] Interactive UI (close button, dragging, resizing)

## Session Log

**2026-09-24**: 
- Continued Phase 2 FFI implementation
- Rewrote `src/decode_videotoolbox.rs` with comprehensive FFI documentation
- All 116 library tests passing
- GPU rendering infrastructure complete and tested
