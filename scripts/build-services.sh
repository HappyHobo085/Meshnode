#!/usr/bin/env bash
# Builds the Meshnode WASM services under services/ for wasm32-wasip2 (release). Each service is
# its own cargo workspace. Deploy the result with `meshd deploy wasm <name> <file>`, e.g.
#   meshd deploy wasm sync services/aegis-sync/target/wasm32-wasip2/release/aegis_sync.wasm
# Extra arguments go to cargo. CARGO_TARGET_DIR, if set, moves the output.
set -euo pipefail
cd "$(dirname "$0")/.."

# Host-only settings from the environment must not leak into the wasm build (the test harness,
# mesh_runtime::test_fixtures, strips the same ones), and the committed lock file is used as is.
unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS CARGO_BUILD_RUSTFLAGS CARGO_BUILD_TARGET
cargo build --locked --release --target wasm32-wasip2 \
    --manifest-path services/aegis-sync/Cargo.toml "$@"
echo "built ${CARGO_TARGET_DIR:-services/aegis-sync/target}/wasm32-wasip2/release/aegis_sync.wasm"
