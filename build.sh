#!/bin/bash
# set -e

if bindgen --version &>/dev/null; then :; else
    cargo install --locked bindgen-cli
fi

OPUS_LIB_DIR=$PWD/../opus-1.5.1/lib \
BUILDKIT_ROOT="$(realpath $PWD/../arm-rockchip830-linux-uclibcgnueabihf)" \
CARGO_TARGET_ARMV7_UNKNOWN_LINUX_UCLIBCEABIHF_LINKER="$BUILDKIT_ROOT/bin/arm-rockchip830-linux-uclibcgnueabihf-gcc" \
cargo build -Z build-std --release --target armv7-unknown-linux-uclibceabihf
echo "test autobuild"