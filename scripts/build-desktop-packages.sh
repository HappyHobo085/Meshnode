#!/usr/bin/env bash
# Builds the desktop packages (spec §3): the Tauri app with `meshd` as its external binary.
#   Linux:   meshnode deb and rpm (the package owns meshnode.service, see
#            apps/desktop/src-tauri/packaging/linux)
#   Windows: the MSI (run from Git Bash)
# Prints the artifact paths. Needs cargo-tauri and the UI's node_modules (beforeBuildCommand
# builds apps/ui). Honours CARGO_TARGET_DIR.
#
#   build-desktop-packages.sh
#
# `bundle.externalBin` is passed here, not set in tauri.conf.json: tauri-build copies every
# external binary in its build script and fails when it is missing, so a plain `cargo build`
# of the desktop app would need a staged meshd first.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TAURI_DIR="$REPO/apps/desktop/src-tauri"

version="$("$REPO/scripts/check-version.sh")"
triple="$(rustc -vV | sed -n 's/^host: //p')"
[ -n "$triple" ] || { echo "cannot read the host target triple from rustc -vV" >&2; exit 1; }

case "$(uname -s)" in
  Linux) platform=linux exe="" bundles=deb,rpm ;;
  MINGW* | MSYS* | CYGWIN*) platform=windows exe=.exe bundles=msi ;;
  *) echo "unsupported platform: $(uname -s)" >&2; exit 1 ;;
esac

# 1. meshd, staged where externalBin expects it: binaries/meshd-<target-triple>[.exe].
(cd "$REPO" && cargo build --release --locked -p meshd)
meshd_target="${CARGO_TARGET_DIR:-$REPO/target}"
mkdir -p "$TAURI_DIR/binaries"
cp "$meshd_target/release/meshd$exe" "$TAURI_DIR/binaries/meshd-$triple$exe"

overlay='{"bundle":{"externalBin":["binaries/meshd"]}}'

if [ "$platform" = linux ]; then
  # The bundlers copy each file's mode from disk (tar HeaderMode::Deterministic for the deb,
  # the source mode for the rpm). A checkout on a filesystem without Unix modes (FAT, NTFS)
  # reports every file as 0755, so the packaged files are staged with explicit modes and
  # the config's paths are pointed at the copies.
  stage="${meshd_target}/meshnode-packaging/linux"
  rm -rf "$stage"
  mkdir -p "$stage"
  cp -R "$TAURI_DIR/packaging/linux/." "$stage/"
  find "$stage" -type d -exec chmod 0755 {} +
  find "$stage" -type f -exec chmod 0644 {} +
  chmod 0755 "$stage"/scripts/*.sh
  # Every path under bundle.linux.{deb,rpm} that names packaging/linux/... is rewritten.
  overlay="$(jq -c --arg stage "$stage" --argjson base "$overlay" '
    def staged: if type == "string" and startswith("packaging/linux/")
      then $stage + "/" + ltrimstr("packaging/linux/") else . end;
    $base * {bundle: {linux: (.bundle.linux | walk(staged))}}
  ' "$TAURI_DIR/tauri.conf.json")"
fi

# 2. The app and its bundles.
(cd "$TAURI_DIR" && cargo tauri build --bundles "$bundles" --config "$overlay")

# 3. Artifact paths.
app_target="${CARGO_TARGET_DIR:-$TAURI_DIR/target}"
shopt -s nullglob
case "$platform" in
  linux) artifacts=("$app_target"/release/bundle/deb/*_"$version"_*.deb
                    "$app_target"/release/bundle/rpm/*-"$version"-*.rpm) ;;
  windows) artifacts=("$app_target"/release/bundle/msi/*_"$version"_*.msi) ;;
esac
[ "${#artifacts[@]}" -gt 0 ] || { echo "no $bundles artifacts for $version found" >&2; exit 1; }
printf '%s\n' "${artifacts[@]}"
