#!/usr/bin/env bash
# Installs the native packages needed to build helgobox on Linux, including the
# egui feature (pot browser) and the test binaries.
#
# The list comes from CONTRIBUTING.adoc ("fresh Ubuntu installation" instructions),
# minus the Rust toolchain itself.
#
# Usage: sudo ./install-build-deps.sh   (or run as root)

set -euo pipefail

if [[ $EUID -ne 0 ]]; then
    echo "This script needs root. Run it with sudo." >&2
    exit 1
fi

apt-get update

apt-get install -y \
    curl \
    git \
    build-essential \
    pkg-config \
    php \
    nasm \
    llvm-dev \
    libclang-dev \
    clang \
    libudev-dev \
    libxdo-dev \
    libx11-dev \
    libxcursor-dev \
    libxcb-dri2-0-dev \
    libxcb-icccm4-dev \
    libx11-xcb-dev \
    mesa-common-dev \
    libgl1-mesa-dev \
    libglu1-mesa-dev \
    libspeechd-dev \
    libgtk-3-dev

echo
echo "All build dependencies installed."
echo "Verify the egui feature builds with:"
echo "  cargo check -p helgobox --features egui"
echo "Run the pot unit tests with:"
echo "  cargo test -p pot -p pot-browser"
