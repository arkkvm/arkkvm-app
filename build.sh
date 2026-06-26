#!/bin/bash
# set -e
#
# External build inputs (not in this repository; see README.md):
#   BUILDKIT_ROOT  - cross toolchain + Rockchip SDK sysroot (proprietary .so)
#   OPUS_LIB_DIR   - prebuilt libopus (BSD-3-Clause)

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APP_CARGO_TOML="$SCRIPT_DIR/arkkvm/Cargo.toml"
USB_CARGO_TOML="$SCRIPT_DIR/crates/usb_devices/Cargo.toml"

# Usage: ./build.sh [version] [dev_channel]
# If a version is provided (e.g. FIRMWARE_VERSION_EFFECTIVE), update [package] version in
# arkkvm/Cargo.toml and crates/usb_devices/Cargo.toml.
if [[ -n "${1:-}" ]]; then
    FIRMWARE_VERSION="$1"
    awk -v ver="$FIRMWARE_VERSION" '
        /^\[package\]$/ { inpkg=1; print; next }
        /^\[/ { inpkg=0 }
        inpkg && /^version = / { print "version = \"" ver "\""; next }
        { print }
    ' "$APP_CARGO_TOML" > "$APP_CARGO_TOML.tmp" && mv "$APP_CARGO_TOML.tmp" "$APP_CARGO_TOML" && {
        pkg_line="$(awk '/^\[package\]$/ { inpkg=1; next } /^\[/ { inpkg=0 } inpkg && /^version = / { print; exit }' "$APP_CARGO_TOML")"
        echo "[ArkKVM App] arkkvm/Cargo.toml: [package] version updated: ${pkg_line}"
    }
    
    awk -v ver="$FIRMWARE_VERSION" '
        /^\[package\]$/ { inpkg=1; print; next }
        /^\[/ { inpkg=0 }
        inpkg && /^version = / { print "version = \"" ver "\""; next }
        { print }
    ' "$USB_CARGO_TOML" > "$USB_CARGO_TOML.tmp" && mv "$USB_CARGO_TOML.tmp" "$USB_CARGO_TOML" && {
        pkg_line="$(awk '/^\[package\]$/ { inpkg=1; next } /^\[/ { inpkg=0 } inpkg && /^version = / { print; exit }' "$USB_CARGO_TOML")"
        echo "[ArkKVM USB] crates/usb_devices/Cargo.toml: [package] version updated: ${pkg_line}"
    }
fi

DEV_CHANNEL="0"
if [[ -n "${2:-}" ]]; then
    DEV_CHANNEL="$2"
fi

if bindgen --version &>/dev/null; then :; else
    cargo install --locked bindgen-cli
fi

export OPUS_LIB_DIR=$PWD/../opus-1.5.1/lib
export BUILDKIT_ROOT="$(realpath $PWD/../arm-rockchip830-linux-uclibcgnueabihf)"
export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_UCLIBCEABIHF_LINKER="$BUILDKIT_ROOT/bin/arm-rockchip830-linux-uclibcgnueabihf-gcc"

if [[ "$DEV_CHANNEL" == "1" ]]; then
    echo "[ArkKVM App] Building for development channel"
    cargo build -Z build-std --features env_dev --release --target armv7-unknown-linux-uclibceabihf
else
    echo "[ArkKVM App] Building for production channel"
    cargo build -Z build-std --release --target armv7-unknown-linux-uclibceabihf
fi

echo "[ArkKVM App] Build Completed"