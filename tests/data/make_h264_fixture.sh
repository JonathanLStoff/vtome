#!/bin/sh
# Rebuilds tests/data/bars_h264.mp4, the fixture tests/videotoolbox.rs decodes.
#
# vtome never writes H.264, so this is the one fixture the tests cannot make for
# themselves; it is committed, and this script is how it was made. It needs
# ffmpeg with libx264 and python3.
#
# 128×96, 24 frames at 24 fps, BT.601 limited range (what vtome guesses for a
# picture this size). Every frame has the same top half — red, green, blue, and
# white bars, 32 px each — so colour can be checked after decoding. The bottom
# half is black with an 8 px white bar at x = 8 + 4·n in frame n, so the order
# frames come out in can be read from the pixels rather than trusted from the
# timestamps.
#
# Two B-frames between every reference frame, fixed (b-adapt=0), so decode
# order and display order genuinely differ and the reordering is exercised.

set -eu

cd "$(dirname "$0")"

python3 - <<'PY' | ffmpeg -hide_banner -loglevel error -y \
    -f rawvideo -pix_fmt rgb24 -s 128x96 -r 24 -i - \
    -vf scale=out_color_matrix=bt601:out_range=tv \
    -c:v libx264 -preset slow -crf 12 -profile:v high \
    -x264-params bframes=2:b-adapt=0:b-pyramid=none:keyint=24:scenecut=0 \
    -pix_fmt yuv420p \
    -colorspace smpte170m -color_primaries smpte170m -color_trc smpte170m -color_range tv \
    -an -movflags +faststart \
    bars_h264.mp4
import sys

WIDTH, HEIGHT, FRAMES = 128, 96, 24
BARS = [(255, 0, 0), (0, 255, 0), (0, 0, 255), (255, 255, 255)]

out = sys.stdout.buffer
for n in range(FRAMES):
    frame = bytearray()
    for y in range(HEIGHT):
        for x in range(WIDTH):
            if y < HEIGHT // 2:
                frame += bytes(BARS[x // 32])
            elif 8 + 4 * n <= x < 16 + 4 * n:
                frame += b"\xff\xff\xff"
            else:
                frame += b"\x00\x00\x00"
    out.write(frame)
PY

echo "wrote $(pwd)/bars_h264.mp4"
