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

# Note the "${arr[@]+...}" guard: macOS ships bash 3.2, where expanding an empty array as
# "${build_args[@]}" under `set -u` errors as an unbound variable.
cargo build -p pot-extension ${build_args[@]+"${build_args[@]}"}

# cargo emits the lib-prefixed cdylib with a platform-specific extension; REAPER expects
# reaper_pot.<ext> (no "lib" prefix). macOS uses .dylib, Linux .so, Windows .dll.
case "$(uname -s)" in
    Darwin)        src=libreaper_pot.dylib; dst=reaper_pot.dylib ;;
    MINGW*|MSYS*|CYGWIN*) src=reaper_pot.dll; dst=reaper_pot.dll ;;
    *)             src=libreaper_pot.so;    dst=reaper_pot.so ;;
esac

cd "target/$profile_dir"
test -f "$src"
mv -f "$src" "$dst"
echo "Built target/$profile_dir/$dst"
