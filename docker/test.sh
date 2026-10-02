#!/bin/sh
# vtome's test matrix, for one kind of system. Every image's entry point, and
# run directly on a Mac for the systems no container can be:
#
#   sh docker/test.sh linux     # Debian, Ubuntu, Fedora, Alpine images
#   sh docker/test.sh windows   # windows-cross image: MinGW build, tests under Wine
#   sh docker/test.sh android   # android image: NDK cross-build
#   sh docker/test.sh macos     # a Mac, natively — plus the iOS cross-build
#
# Every step runs even when an earlier one fails, and the summary at the end
# says which did what. Exits non-zero if any step failed.

set -u

profile="${1:-linux}"

# Always a copy of the source — /src in a container, the checkout on a Mac — so
# the fixture rebuilt below never replaces the committed one.
#
# On a Mac the copy builds into a target directory of its own, kept between
# runs but never shared with the checkout's. Sharing it looked free and was
# not: the copy's binaries have the copy's path compiled in
# (`env!("CARGO_MANIFEST_DIR")`), cargo sees byte-identical sources and keeps
# them, and the next plain `cargo test` in the checkout ran tests that looked
# for fixtures in a temp directory long since deleted. This matrix is for
# special occasions; it must never change what `make test` does.
if [ -d /src ]; then
    source=/src
else
    source="$(cd "$(dirname "$0")/.." && pwd)"
    export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$source/target/docker-macos}"
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
tar -C "$source" --exclude=./target --exclude=./.git -cf - . | tar -C "$work" -xf -
cd "$work"

results=""
failed=0

step() {
    name="$1"
    shift
    echo ""
    echo "==> $name: $*"
    if "$@"; then
        results="$results
  PASS  $name"
    else
        results="$results
  FAIL  $name"
        failed=1
    fi
}

# The converters: rebuild the H.264 fixture with this system's ffmpeg, in the
# copy, so the decode tests run against a file made here — which checks the
# image and the fixture script at once.
converters() {
    ffmpeg -hide_banner -version | head -1 && sh tests/data/make_h264_fixture.sh
}

skip() {
    echo ""
    echo "==> $1: skipped — $2"
    results="$results
  SKIP  $1 ($2)"
}

case "$profile" in
    linux)
        step converters converters
        step default cargo test
        step no-default-features cargo test --no-default-features
        # Mesa's software Vulkan: the GPU tests draw and read back for real.
        step render cargo test --features render
        # The engine's own tests; opening windows needs a display, which the
        # container has not got, so the examples are only built.
        step window cargo test --features window --lib
        step window-examples cargo check --features window --all-targets
        # VA-API is not implemented yet, so decode-platform builds here and
        # its decode tests say "not implemented" rather than decode.
        step decode-platform cargo test --features render,decode-platform
        step tauri cargo check --features tauri
        ;;

    windows)
        target=x86_64-pc-windows-gnu
        step converters converters
        step build cargo build --target "$target" --features render,window
        step default cargo test --target "$target"
        step render cargo test --target "$target" --features render
        step tauri cargo check --target "$target" --features tauri
        ;;

    android)
        target=aarch64-linux-android
        # winit needs an activity chosen by the application on Android, so the
        # `window` feature is the application's to build, not the library's.
        step default cargo build --target "$target"
        step render cargo build --target "$target" --features render
        step decode-platform cargo build --target "$target" --features render,decode-platform
        ;;

    macos)
        step converters converters
        step default cargo test
        step no-default-features cargo test --no-default-features
        # A real GPU and VideoToolbox: every test here draws and decodes.
        step render-and-videotoolbox cargo test --features render,decode-platform
        step window cargo test --features window,decode-platform --lib
        step window-examples cargo check --features window,decode-platform --all-targets
        step tauri cargo check --features tauri,decode-platform
        if rustup target list --installed 2>/dev/null | grep -q aarch64-apple-ios; then
            step ios cargo build --target aarch64-apple-ios --features render,decode-platform
        else
            skip ios "rustup target add aarch64-apple-ios to include it"
        fi
        ;;

    *)
        echo "unknown profile '$profile': linux, windows, android, or macos" >&2
        exit 2
        ;;
esac

echo ""
echo "vtome on $profile$( [ -f /etc/os-release ] && . /etc/os-release && printf ' (%s)' "$PRETTY_NAME"):$results"

exit "$failed"
