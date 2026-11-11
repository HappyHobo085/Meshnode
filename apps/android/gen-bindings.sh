#!/usr/bin/env bash
# Regenerates the UniFFI Kotlin bindings of mesh-ffi (package net.meshnode.ffi) into
# src-tauri/gen/android/app/src/main/java/net/meshnode/ffi/, from an UNSTRIPPED
# libmeshnode_app.so (the bindings are read from the library's UniFFI metadata, which a
# stripped release library has lost).
#
# Usage: apps/android/gen-bindings.sh [path/to/libmeshnode_app.so]
#
# Without an argument it uses the debug arm64 library of the last
# `cargo tauri android build --debug` (in $CARGO_TARGET_DIR, else the default target dir),
# and builds it with cargo-ndk if it is missing (needs ANDROID_NDK_HOME or NDK_HOME).
#
# The generator is mesh-ffi's `uniffi-bindgen` binary, run from the root workspace so that
# crates/mesh-ffi/uniffi.toml applies (package net.meshnode.ffi, cdylib meshnode_app).
# Re-run it whenever mesh-ffi's exported interface changes, and commit the result.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
manifest="$here/src-tauri/Cargo.toml"
out="$here/src-tauri/gen/android/app/src/main/java"

target_dir="${CARGO_TARGET_DIR:-}"
if [ -z "$target_dir" ]; then
    target_dir="$(cargo metadata --manifest-path "$manifest" --format-version 1 --no-deps |
        sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' | tr '\\\\' '/' | tr -s '/')"
fi

lib="${1:-$target_dir/aarch64-linux-android/debug/libmeshnode_app.so}"
if [ ! -f "$lib" ]; then
    echo "building $lib with cargo-ndk" >&2
    cargo ndk -t arm64-v8a build --manifest-path "$manifest" --lib
fi

# ktlint formats the output when it is installed; otherwise keep it unformatted.
# UNIFFI_NO_FORMAT=1 forces unformatted output (CI, so a ktlint on the runner cannot cause drift).
format=(--no-format)
if [ "${UNIFFI_NO_FORMAT:-}" != 1 ] && command -v ktlint >/dev/null 2>&1; then
    format=()
fi

rm -rf "$out/net/meshnode/ffi"
cd "$root"
cargo run -p mesh-ffi --features bindgen --bin uniffi-bindgen -- \
    generate --library "$lib" --language kotlin --out-dir "$out" "${format[@]}"

echo "wrote $(find "$out/net/meshnode/ffi" -name '*.kt' | tr '\n' ' ')" >&2
