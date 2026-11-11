#!/usr/bin/env bash
# Regenerates the TypeScript bindings of mesh-ui (ts-rs) into apps/ui/src/lib/generated.
# Always generates into a fresh temp dir, so stale files never survive.
#   (no args)  replace the generated dir with the fresh output
#   --check    fail on any added, removed or changed file versus the generated dir
set -euo pipefail
cd "$(dirname "$0")/.."
DEST="$PWD/apps/ui/src/lib/generated"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
export TS_RS_EXPORT_DIR="$TMP/generated"
export CARGO_INCREMENTAL=0
mkdir -p "$TS_RS_EXPORT_DIR"
cargo test -p mesh-ui export_bindings -j 2
if [ -z "$(ls -A "$TS_RS_EXPORT_DIR")" ]; then
  echo "no bindings were generated" >&2
  exit 1
fi
if [ "${1:-}" = "--check" ]; then
  # Same line endings on every platform (.gitattributes pins the committed files to LF).
  find "$TS_RS_EXPORT_DIR" -type f -exec sed -i 's/\r$//' {} +
  if [ ! -d "$DEST" ]; then
    echo "$DEST is missing" >&2
    exit 1
  fi
  mkdir -p "$TMP/committed"
  cp -r "$DEST"/. "$TMP/committed"/
  find "$TMP/committed" -type f -exec sed -i 's/\r$//' {} +
  diff -r "$TMP/committed" "$TS_RS_EXPORT_DIR"
else
  rm -rf "$DEST"
  mkdir -p "$DEST"
  cp -r "$TS_RS_EXPORT_DIR"/. "$DEST"/
fi
