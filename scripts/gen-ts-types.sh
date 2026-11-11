#!/usr/bin/env bash
# Regenerates the TypeScript bindings of mesh-ui (ts-rs) into apps/ui/src/lib/generated.
# With --check, fails when the committed files differ from the generated ones.
set -euo pipefail
cd "$(dirname "$0")/.."
export TS_RS_EXPORT_DIR="$PWD/apps/ui/src/lib/generated"
export CARGO_INCREMENTAL=0
cargo test -p mesh-ui export_bindings -j 2
if [ "${1:-}" = "--check" ]; then
  git -c safe.directory="$PWD" diff --exit-code apps/ui/src/lib/generated
fi
