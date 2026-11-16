#!/usr/bin/env bash
# Runs the JVM test of the Kotlin bindings: builds the host library, then `gradlew test`
# with JAVA_HOME's JDK (17+). Gradle resolves its dependencies into the user's Gradle cache.
#
# Usage: crates/mesh-client-ffi/jvm-test/run.sh [extra gradle args]
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../../.." && pwd)"
cd "$root"
target_dir="${CARGO_TARGET_DIR:-}"
if [ -z "$target_dir" ]; then
    target_dir="$(cargo metadata --format-version 1 --no-deps |
        sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' | tr '\\' '/' | tr -s '/')"
fi
cargo build -p mesh-client-ffi --lib
cd "$here"
./gradlew --no-daemon test -PnativeDir="$target_dir/debug" "$@"
