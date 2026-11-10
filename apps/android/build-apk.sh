#!/usr/bin/env bash
# Builds the arm64 APK: `cargo tauri android build --target aarch64 --apk`, debug unless
# `--release` is given. Output: src-tauri/gen/android/app/build/outputs/apk/universal/<profile>/.
#
# Usage: apps/android/build-apk.sh [--release]
# Needs JAVA_HOME (JDK 17), ANDROID_HOME, NDK_HOME (NDK 27.2) and the Tauri CLI
# (`cargo install tauri-cli --version "^2" --locked`).
#
# The Tauri CLI symlinks the built libmeshnode_app.so into app/src/main/jniLibs/. That fails
# where symbolic links cannot be made: Windows without Developer Mode, or a checkout on FAT32.
# Then this script does the remaining steps itself, the way the CLI does them: it copies the
# library (already built by the CLI, with the Kotlin glue it generates) into jniLibs and runs
# the same Gradle task, skipping the Gradle Rust tasks (they would rebuild and symlink again).
# The fallback is detected by the CLI's error text "Failed to create a symbolic link", as
# printed by tauri-cli 2.12.4 (cargo-mobile2 0.22.5); recheck it when the CLI is upgraded.
set -euo pipefail

profile=debug
cli_flags=(--debug)
if [ "${1:-}" = "--release" ]; then
    profile=release
    cli_flags=()
fi
Profile="${profile^}"

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
tauri_dir="$here/src-tauri"
android_dir="$tauri_dir/gen/android"
log="$(mktemp)"
trap 'rm -f "$log"' EXIT

echo "build-apk: $(cargo tauri --version)" >&2
cd "$tauri_dir"
if cargo tauri android build "${cli_flags[@]}" --target aarch64 --apk --ci 2>&1 | tee "$log"; then
    exit 0
fi
if ! grep -q "Failed to create a symbolic link" "$log"; then
    exit 1
fi
echo "build-apk: symbolic links unavailable; copying the library into jniLibs instead" >&2

target_dir="${CARGO_TARGET_DIR:-}"
if [ -z "$target_dir" ]; then
    target_dir="$(cargo metadata --manifest-path "$tauri_dir/Cargo.toml" --format-version 1 --no-deps |
        sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' | tr '\\\\' '/' | tr -s '/')"
fi
jni="$android_dir/app/src/main/jniLibs/arm64-v8a"
mkdir -p "$jni"
rm -f "$jni/libmeshnode_app.so"
cp "$target_dir/aarch64-linux-android/$profile/libmeshnode_app.so" "$jni/"

cd "$android_dir"
./gradlew "assembleUniversal$Profile" \
    -PabiList=arm64-v8a -ParchList=arm64 -PtargetList=aarch64 \
    -x "rustBuildArm64$Profile" -x "rustBuildUniversal$Profile" --warn
