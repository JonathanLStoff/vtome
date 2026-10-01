# vtome's Linux test image, Debian family: `BASE=debian:bookworm` or
# `BASE=ubuntu:24.04`. Built and run by docker/run.sh.
#
# Everything the default toolkit and its feature matrix need from the system,
# and the converters that make the test fixtures:
#
#   - a Rust toolchain and a C toolchain
#   - ffmpeg (with x264, AAC, MP3, Opus) and python3 — the converters. The
#     H.264 fixture is rebuilt with them inside the container, which proves
#     both the image and tests/data/make_h264_fixture.sh
#   - Mesa's software Vulkan (lavapipe), so the GPU tests draw and read back
#     real pixels with no GPU in the container
#   - winit's X11/Wayland headers, and GTK and WebKitGTK for the `tauri`
#     feature — Tauri 2 needs them on Linux even with no webview in use
#
# The source is mounted read-only at /src and copied, so fixtures made here
# never land in the working tree.

ARG BASE=debian:bookworm
FROM ${BASE}

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl git build-essential pkg-config clang cmake \
        python3 ffmpeg \
        mesa-vulkan-drivers libvulkan1 libegl1 \
        libx11-dev libxkbcommon-dev libwayland-dev libxcursor-dev libxrandr-dev libxi-dev \
        libgtk-3-dev libwebkit2gtk-4.1-dev libsoup-3.0-dev libjavascriptcoregtk-4.1-dev \
        libasound2-dev \
    && rm -rf /var/lib/apt/lists/*

RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable

# Where Mesa and winit look for a display's runtime files. There is no display,
# but without the directory every GPU test prints a complaint about it.
RUN mkdir -p -m 700 /run/user/0

# The container has 4 GB; wgpu and tauri compiled eight crates at a time do not
# fit in it.
ENV XDG_RUNTIME_DIR=/run/user/0 \
    PATH=/root/.cargo/bin:$PATH \
    CARGO_BUILD_JOBS=4 \
    CARGO_TARGET_DIR=/target \
    CARGO_TERM_COLOR=never

WORKDIR /work
CMD ["sh", "/src/docker/test.sh", "linux"]
