#!/usr/bin/env bash
# Builds the Meshnode WASM services under services/ for wasm32-wasip2 (release). Each service is
# its own cargo workspace. Deploy the result with `meshd deploy wasm <name> <file>`, e.g.
#   meshd deploy wasm sync services/aegis-sync/target/wasm32-wasip2/release/aegis_sync.wasm
# Extra arguments go to cargo (e.g. --locked). CARGO_TARGET_DIR, if set, moves the output.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release --target wasm32-wasip2 --manifest-path services/aegis-sync/Cargo.toml "$@"
echo "built ${CARGO_TARGET_DIR:-services/aegis-sync/target}/wasm32-wasip2/release/aegis_sync.wasm"
