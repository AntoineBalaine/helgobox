#!/usr/bin/env bash
# Builds the standalone Pot REAPER extension and renames the cargo output to the name
# REAPER expects (cargo always emits the lib-prefixed libreaper_pot.so for a cdylib).
#
# Usage: ./build-pot-extension.sh [--release]

set -euo pipefail

profile_dir="debug"
build_args=()
if [[ "${1:-}" == "--release" ]]; then
    build_args=(--profile release-strip)
    profile_dir="release-strip"
fi

cargo build -p pot-extension "${build_args[@]}"

cd "target/$profile_dir"
test -f libreaper_pot.so
mv -f libreaper_pot.so reaper_pot.so
echo "Built target/$profile_dir/reaper_pot.so"
