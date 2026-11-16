#!/usr/bin/env bash
# Sets the single Meshnode product version (x.y.z) in the five files check-version.sh verifies:
# the [package] version of each Cargo.toml and the top-level "version" of each tauri.conf.json.
#
#   set-version.sh <x.y.z> [root]    rewrite all five files; `root` defaults to the repo root
#
# Android versionCode: nothing to set here. Tauri 2 derives tauri.android.versionCode from
# `version` when bundle.android.versionCode is unset and autoIncrementVersionCode is false:
#   versionCode = major*1000000 + minor*1000 + patch
# (tauri-cli 2.12.4, src/mobile/android/mod.rs:1173; documented at tauri-utils 2.10.2
# src/config.rs:3662). Limits: minor and patch must stay <= 999 or codes collide with the next
# minor/major (not enforced by Tauri), the code must be 1..2100000000 (Tauri bails otherwise),
# and 0.0.0 is rejected. This script enforces minor, patch <= 999 and major <= 2100.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CARGO_FILES=(apps/meshd/Cargo.toml apps/desktop/src-tauri/Cargo.toml apps/android/src-tauri/Cargo.toml)
JSON_FILES=(apps/desktop/src-tauri/tauri.conf.json apps/android/src-tauri/tauri.conf.json)

ver="${1:-}"
root="${2:-$REPO}"
if ! [[ "$ver" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
  echo "usage: set-version.sh <x.y.z> [root]  (got '${ver}')" >&2
  exit 2
fi
IFS=. read -r major minor patch <<<"$ver"
if [ "$major" -gt 2100 ] || [ "$minor" -gt 999 ] || [ "$patch" -gt 999 ] || [ "$ver" = "0.0.0" ]; then
  echo "version $ver out of range for the derived Android versionCode (see header)" >&2
  exit 2
fi

for f in "${CARGO_FILES[@]}"; do
  sed -i "/^\[package\]/,/^\[/ s/^version = \".*\"/version = \"$ver\"/" "$root/$f"
done
for f in "${JSON_FILES[@]}"; do
  sed -i "0,/^  \"version\": \".*\"/ s//  \"version\": \"$ver\"/" "$root/$f"
done
echo "set version $ver"
