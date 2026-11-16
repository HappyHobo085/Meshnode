#!/usr/bin/env bash
# Verifies the single Meshnode product version across the five files that carry it:
#   apps/meshd/Cargo.toml, apps/{desktop,android}/src-tauri/{Cargo.toml,tauri.conf.json}
#
#   check-version.sh [root]    print the version and exit 0 when all five agree; otherwise
#                              exit 1 naming each file that differs from apps/meshd/Cargo.toml
#   check-version.sh --self-test   exercise the check on a temp copy of the tree
#
# `root` defaults to the repo root. set-version.sh writes the files this script checks.
set -euo pipefail

SELF="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FILES=(
  apps/meshd/Cargo.toml
  apps/desktop/src-tauri/Cargo.toml
  apps/desktop/src-tauri/tauri.conf.json
  apps/android/src-tauri/Cargo.toml
  apps/android/src-tauri/tauri.conf.json
)

# version_of <file>: the [package] version of a Cargo.toml, or the top-level "version" of a json.
version_of() {
  case "$1" in
    *.toml) awk '/^\[/ { in_pkg = ($0 == "[package]") } in_pkg && /^version[ ]*=/ { gsub(/^[^"]*"|".*$/, ""); print; exit }' "$1" ;;
    *.json) sed -n 's/^  "version": "\([^"]*\)".*/\1/p' "$1" | head -n1 ;;
  esac
}

check() {
  local root="$1" ref f v bad=0
  ref="$(version_of "$root/${FILES[0]}")"
  [ -n "$ref" ] || { echo "no version found in ${FILES[0]}" >&2; return 1; }
  for f in "${FILES[@]}"; do
    v="$(version_of "$root/$f")"
    if [ "$v" != "$ref" ]; then
      echo "version mismatch: $f has '${v:-<none>}', expected '$ref' (from ${FILES[0]})" >&2
      bad=1
    fi
  done
  [ "$bad" -eq 0 ] || return 1
  echo "$ref"
}

self_test() {
  local tmp f
  tmp="$(mktemp -d)"
  trap "rm -rf '$tmp'" EXIT
  for f in "${FILES[@]}"; do
    mkdir -p "$tmp/$(dirname "$f")"
    cp "$REPO/$f" "$tmp/$f"
  done
  bash "$SELF" "$tmp" >/dev/null || { echo "FAIL: pristine copy should pass" >&2; exit 1; }
  sed -i '0,/^version = ".*"/s//version = "9.9.9"/' "$tmp/apps/android/src-tauri/Cargo.toml"
  if out="$(bash "$SELF" "$tmp" 2>&1)"; then echo "FAIL: mismatch not detected" >&2; exit 1; fi
  case "$out" in *apps/android/src-tauri/Cargo.toml*) ;; *) echo "FAIL: file not named: $out" >&2; exit 1 ;; esac
  cp "$REPO/apps/android/src-tauri/Cargo.toml" "$tmp/apps/android/src-tauri/Cargo.toml"
  bash "$SELF" "$tmp" >/dev/null || { echo "FAIL: restored copy should pass" >&2; exit 1; }
  echo "check-version self-test ok"
}

case "${1:-}" in
  --self-test) self_test ;;
  -h|--help) sed -n '2,9p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 2 ;;
  *) check "${1:-$REPO}" ;;
esac
