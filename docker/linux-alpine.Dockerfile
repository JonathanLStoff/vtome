# vtome's Linux test image, Alpine: musl rather than glibc. Built and run by
# docker/run.sh; see linux-debian.Dockerfile for what is in it and why.

FROM alpine:3.20

RUN apk add --no-cache \
        ca-certificates curl git bash tar build-base pkgconf clang cmake \
        python3 ffmpeg \
        mesa-vulkan-swrast vulkan-loader mesa-egl \
        libx11-dev libxkbcommon-dev wayland-dev libxcursor-dev libxrandr-dev libxi-dev \
        gtk+3.0-dev webkit2gtk-4.1-dev libsoup3-dev \
        alsa-lib-dev

RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable

# Static musl binaries cannot dlopen, and wgpu finds Vulkan by dlopening it.
# Where Mesa and winit look for a display's runtime files. There is no display,
# but without the directory every GPU test prints a complaint about it.
RUN mkdir -p -m 700 /run/user/0

ENV XDG_RUNTIME_DIR=/run/user/0 \
    PATH=/root/.cargo/bin:$PATH \
    RUSTFLAGS="-C target-feature=-crt-static" \
    CARGO_BUILD_JOBS=4 \
    CARGO_TARGET_DIR=/target \
    CARGO_TERM_COLOR=never

WORKDIR /work
CMD ["sh", "/src/docker/test.sh", "linux"]
