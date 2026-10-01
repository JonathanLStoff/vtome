# vtome's Linux test image, Fedora. Built and run by docker/run.sh; see
# linux-debian.Dockerfile for what is in it and why.
#
# Fedora's own ffmpeg is built without x264 and the other encumbered encoders,
# so the converters come from RPM Fusion — only for making fixtures; vtome
# itself never links ffmpeg.

FROM fedora:41

RUN dnf install -y \
        https://mirrors.rpmfusion.org/free/fedora/rpmfusion-free-release-41.noarch.rpm \
    && dnf install -y --allowerasing \
        ca-certificates curl git gcc gcc-c++ make pkgconf-pkg-config clang cmake \
        python3 ffmpeg \
        mesa-vulkan-drivers vulkan-loader mesa-libEGL \
        libX11-devel libxkbcommon-devel wayland-devel libXcursor-devel libXrandr-devel libXi-devel \
        gtk3-devel webkit2gtk4.1-devel libsoup3-devel javascriptcoregtk4.1-devel \
        alsa-lib-devel \
    && dnf clean all

RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable

# Where Mesa and winit look for a display's runtime files. There is no display,
# but without the directory every GPU test prints a complaint about it.
RUN mkdir -p -m 700 /run/user/0

ENV XDG_RUNTIME_DIR=/run/user/0 \
    PATH=/root/.cargo/bin:$PATH \
    CARGO_BUILD_JOBS=4 \
    CARGO_TARGET_DIR=/target \
    CARGO_TERM_COLOR=never

WORKDIR /work
CMD ["sh", "/src/docker/test.sh", "linux"]
