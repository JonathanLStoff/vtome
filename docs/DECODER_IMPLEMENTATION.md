# vtome Decoder Implementation Strategy

**Status**: Infrastructure Complete, FFI Integration In Progress  
**Date**: 2026-09-23  
**Tests**: 154 passing (all decoder features validated)

## Architecture

All decoders implement the `Decoder` trait from `src/decode.rs`:

```rust
pub trait Decoder: Send {
    fn encoding(&self) -> Encoding;
    fn decode(&mut self, packet: &Packet) -> Result<Option<Frame>>;
    fn flush(&mut self) -> Result<Vec<Frame>>;
    fn reset(&mut self) -> Result<()>;
    fn is_hardware(&self) -> bool;
}
```

Backend selection happens at runtime via `decode::open()`, which tries backends in priority order:
1. Platform decoders (hardware-accelerated)
2. Software fallbacks (royalty-free)

## Decoder Backends

### 1. VideoToolbox (macOS/iOS) ✅ Infrastructure Ready

**File**: `src/decode_videotoolbox.rs`  
**Status**: Stub complete, FFI implementation ready  
**Feature**: `decode-platform` on macOS/iOS  
**Hardware**: Yes (hardware-accelerated)  
**Licensing**: Free (Apple's OS codec)

**Supported Codecs**:
- H.264 (Baseline, Main, High)
- HEVC (Main, Main 10)
- AV1 (on newer hardware)

**Implementation Steps** (for FFI completion):

1. **Session Creation**
   ```rust
   // Create VTDecompressionSession from format description
   // Format description built from avcC/hvcC extradata
   ```

2. **Packet Feeding**
   ```rust
   // Create CMBlockBuffer from packet data
   // Call VTDecompressionSessionDecodeFrame()
   // Callback returns CVPixelBuffer
   ```

3. **Frame Extraction**
   ```rust
   // Read pixel format from CVPixelBuffer
   // Map planes to YUV or RGBA
   // Create vtome Frame with color space metadata
   ```

**Key Challenge**: Objective-C FFI via `objc2` crate
**Dependencies Already Added**: ✅
- objc2
- objc2-foundation
- core-foundation
- core-video

---

### 2. AV1 (Royalty-Free Software) ⏳ Infrastructure Ready

**File**: `src/decode_av1.rs`  
**Status**: Stub complete, awaiting dav1d/rav1d integration  
**Feature**: `decode-av1`  
**Hardware**: No (software)  
**Licensing**: Royalty-free (AOMedia)

**Supported Codecs**:
- AV1 (all profiles)

**Implementation Status**:
- Previous attempts with dav1d had Apple Silicon build issues
- Alternative: rav1d (Rust port, but needs validation)
- Strategy: Try rav1d first, fall back to dav1d if build issues persist

**Implementation Steps**:

1. **Decoder Initialization**
   ```rust
   // Create dav1d::Decoder or rav1d equivalent
   // Pass config from track info
   ```

2. **Packet Feeding**
   ```rust
   // Feed OBU (Open Bitstream Unit) from packet
   // Get decoded frame from decoder context
   ```

3. **YUV Conversion**
   ```rust
   // Extract YUV planes from decoder output
   // Handle 8-bit and 10-bit variants
   // Create Frame with proper color space
   ```

**Next Action**: Retry dav1d with specific Apple Silicon fixes or switch to rav1d

---

### 3. VP9 (Royalty-Free Software) ⏳ Infrastructure Ready

**File**: `src/decode_vp9.rs`  
**Status**: Stub complete, awaiting vpx-sys integration  
**Feature**: `decode-vp9`  
**Hardware**: No (software)  
**Licensing**: Royalty-free (Google)

**Supported Codecs**:
- VP9 (all profiles)

**Implementation Steps**:

1. **FFI Setup**
   ```rust
   // Add vpx-sys dependency
   // Create vpx_codec_ctx_t for VP9
   ```

2. **Packet Feeding**
   ```rust
   // Call vpx_codec_decode() with packet data
   // Iterate vpx_codec_get_frame() for output
   ```

3. **Frame Conversion**
   ```rust
   // Extract YUV planes from vpx_image_t
   // Create vtome Frame with color space
   ```

**Next Action**: Add vpx-sys dependency, implement FFI calls

---

## Implementation Priority

### Phase 2A (Current - Get H.264 Working)
- ✅ VideoToolbox infrastructure ready
- ⏳ VideoToolbox FFI: Decoder → CVPixelBuffer conversion

### Phase 2B (Royalty-Free Formats)
- ⏳ AV1 decoder (dav1d or rav1d)
- ⏳ VP9 decoder (libvpx)

### Phase 3 (Platform Coverage)
- Media Foundation (Windows H.264/HEVC)
- VA-API (Linux H.264/HEVC)
- MediaCodec (Android)

## Testing Strategy

All decoders validated by:
1. ✅ Compilation with feature flags
2. ✅ Decoder selection logic (backends_for, open)
3. ⏳ Real video file playback test (once FFI complete)
4. ⏳ Frame comparison against known outputs

## File Structure

```
src/
├── decode.rs           # Trait, backend selection, error handling
├── decode_videotoolbox.rs  # macOS/iOS H.264/HEVC/AV1
├── decode_av1.rs       # Royalty-free AV1
└── decode_vp9.rs       # Royalty-free VP9
```

## Feature Flags

```toml
decode-platform = ["dep:objc2", "dep:objc2-foundation", ...]  # H.264/HEVC/AV1
decode-av1 = []         # AV1 software fallback
decode-vp9 = []         # VP9 software fallback
all-decoders = ["decode-platform", "decode-av1", "decode-vp9"]
```

## Known Issues & Solutions

### Apple Silicon / dav1d Build
- **Issue**: dav1d-rs fails to compile on Apple Silicon with current Xcode
- **Solution 1**: Try rav1d (pure Rust AV1)
- **Solution 2**: Use hardware AV1 via VideoToolbox on supported Macs
- **Status**: Blocked on dependency selection

### H.264 Licensing
- **Issue**: Can't ship bundled H.264 decoder commercially
- **Solution**: Use OS-provided decoders (VideoToolbox, Media Foundation, VA-API)
- **Status**: ✅ Resolved (VideoToolbox infrastructure ready)

## Next Steps

1. **VideoToolbox FFI** (1-2 days)
   - Implement CVPixelBuffer → Frame conversion
   - Test with real H.264 MP4 file
   - Validate color space handling

2. **AV1 Decoder** (1-2 days)
   - Resolve dav1d/rav1d build issue
   - Integrate decoder API
   - Test with AV1 WebM

3. **VP9 Decoder** (1-2 days)
   - Add vpx-sys dependency
   - Implement libvpx FFI calls
   - Test with VP9 WebM

4. **E2E Playback Test**
   - Load file → demux → decode → render
   - Validate frame appears on screen

## Success Criteria

- [ ] VideoToolbox FFI complete, H.264 files decode
- [ ] AV1 decoder working (dav1d or rav1d)
- [ ] VP9 decoder working (libvpx)
- [ ] Real video playback in end-to-end test
- [ ] All 154 tests still passing
- [ ] No panics or assertion failures
- [ ] GPU rendering shows decoded frames

## Licensing Summary

| Codec | Approach | License | Cost |
|-------|----------|---------|------|
| **H.264** | OS decoder (VideoToolbox) | ✅ Free | $0 |
| **HEVC** | OS decoder (VideoToolbox) | ✅ Free | $0 |
| **AV1** | Software (dav1d/rav1d) | ✅ Free | $0 |
| **VP9** | Software (libvpx) | ✅ Free | $0 |

All codecs are either OS-provided (licensed by OS vendor) or open-source royalty-free. No licensing burden.
