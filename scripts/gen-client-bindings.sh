#!/usr/bin/env bash
# Regenerates the UniFFI Kotlin bindings of mesh-client-ffi (package net.meshnode.client)
# into crates/mesh-client-ffi/kotlin/net/meshnode/client/, from the HOST library (the
# bindings are read from the library's UniFFI metadata, which is the same on every target).
#
# Usage: scripts/gen-client-bindings.sh [--check]
#   --check  generate into a temporary directory and fail if the committed copy differs
#            (CI: the committed bindings must be current).
#
# The generator is mesh-client-ffi's `uniffi-bindgen` binary, run from the root workspace
# so that crates/mesh-client-ffi/uniffi.toml applies. Re-run this whenever the exported
# interface changes, and commit the result. As apps/android/gen-bindings.sh, ktlint formats
# the output when it is installed unless UNIFFI_NO_FORMAT=1 (CI, and --check, which also
# compares unformatted output; commit unformatted bindings).
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="$root/crates/mesh-client-ffi/kotlin"
pkg="net/meshnode/client"

check=0
case "${1:-}" in
    "") ;;
    --check) check=1 ;;
    *) echo "usage: $0 [--check]" >&2; exit 2 ;;
esac

cd "$root"
target_dir="${CARGO_TARGET_DIR:-}"
if [ -z "$target_dir" ]; then
    target_dir="$(cargo metadata --format-version 1 --no-deps |
        sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' | tr '\\' '/' | tr -s '/')"
fi
case "$(uname -s)" in
    MINGW* | MSYS* | CYGWIN* | Windows_NT) lib="$target_dir/debug/mesh_client_ffi.dll" ;;
    Darwin) lib="$target_dir/debug/libmesh_client_ffi.dylib" ;;
    *) lib="$target_dir/debug/libmesh_client_ffi.so" ;;
esac

cargo build -p mesh-client-ffi --lib

format=(--no-format)
if [ "$check" = 0 ] && [ "${UNIFFI_NO_FORMAT:-}" != 1 ] && command -v ktlint >/dev/null 2>&1; then
    format=()
fi

gen() {
    cargo run -p mesh-client-ffi --features bindgen --bin uniffi-bindgen -- \
        generate --library "$lib" --language kotlin --out-dir "$1" "${format[@]}"
}

if [ "$check" = 1 ]; then
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    gen "$tmp"
    # --strip-trailing-cr: a Windows checkout may have CRLF line ends.
    if ! diff -r --strip-trailing-cr "$tmp/$pkg" "$out/$pkg"; then
        echo "the committed Kotlin bindings are out of date: run scripts/gen-client-bindings.sh" >&2
        exit 1
    fi
    echo "Kotlin bindings are current" >&2
else
    rm -rf "${out:?}/$pkg"
    gen "$out"
    echo "wrote $(find "$out/$pkg" -name '*.kt' | tr '\n' ' ')" >&2
fi
