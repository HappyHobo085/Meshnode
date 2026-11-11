#!/usr/bin/env bash
# Vendors the Aegis sync core (sync-server/core of the Aegis repo) into vendor/aegis-sync-core,
# byte-identical to one Aegis commit, which is recorded in vendor/VENDORED.md.
#
#   sync-aegis-core.sh <aegis-checkout>           copy: replace vendor/aegis-sync-core with the
#                                                 core at the checkout's HEAD; record the commit
#   sync-aegis-core.sh <aegis-checkout> --check   diff the vendored tree against the core at the
#                                                 commit recorded in VENDORED.md (exact bytes)
#   sync-aegis-core.sh --check                    only validate VENDORED.md (40-hex commit present)
#   sync-aegis-core.sh --self-test                exercise copy and --check on a temp git repo
#
# Files come out of `git archive`, never the working tree, so Windows line-ending conversion
# (core.autocrlf) cannot alter them. .gitattributes keeps them untouched on the Meshnode side too.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC_PATH="sync-server/core"
DEST_REL="vendor/aegis-sync-core"
NOTE_REL="vendor/VENDORED.md"

usage() {
  sed -n '2,12p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' >&2
  exit 2
}

# `*` so a checkout on a filesystem without ownership info (FAT32, Windows) is still readable.
aegis_git() { git -c safe.directory='*' -C "$AEGIS" "$@"; }

recorded_hash() {
  local note="$ROOT/$NOTE_REL" h
  [ -f "$note" ] || { echo "missing $NOTE_REL" >&2; return 1; }
  h="$(grep -oE '(^|[^0-9a-fA-F])[0-9a-f]{40}([^0-9a-fA-F]|$)' "$note" | head -n1 | grep -oE '[0-9a-f]{40}' || true)"
  [ -n "$h" ] || { echo "$NOTE_REL has no 40-hex commit hash" >&2; return 1; }
  printf '%s\n' "$h"
}

# Extract <commit>:sync-server/core into $2 (an existing empty dir), at the tree root.
extract() {
  aegis_git archive --format=tar "$1" "$SRC_PATH" | tar -x -C "$2" --strip-components=2
}

do_copy() {
  local hash tmp
  hash="$(aegis_git rev-parse --verify HEAD^{commit})"
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN
  extract "$hash" "$tmp"
  [ -f "$tmp/Cargo.toml" ] || { echo "no $SRC_PATH/Cargo.toml at $hash" >&2; return 1; }
  rm -rf "${ROOT:?}/$DEST_REL"
  mkdir -p "$ROOT/$DEST_REL"
  cp -R "$tmp/." "$ROOT/$DEST_REL/"
  cat > "$ROOT/$NOTE_REL" <<NOTE
# Vendored code

## aegis-sync-core

- Source: Aegis repository, \`$SRC_PATH\`
- Commit: $hash
- Licence: AGPL-3.0-only (same as Meshnode)
- Destination: \`$DEST_REL\`

The tree is a byte-identical copy of that commit. Never edit it here; change Aegis, then run
\`scripts/sync-aegis-core.sh <aegis-checkout>\`. \`scripts/sync-aegis-core.sh <aegis-checkout> --check\`
verifies it, and \`--check\` alone validates this file.
NOTE
  echo "vendored $SRC_PATH at $hash"
}

do_check() {
  local hash tmp
  hash="$(recorded_hash)"
  if [ -z "${AEGIS:-}" ]; then
    echo "ok: $NOTE_REL records $hash"
    return 0
  fi
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN
  aegis_git cat-file -e "$hash^{commit}" || { echo "commit $hash is not in $AEGIS" >&2; return 1; }
  extract "$hash" "$tmp"
  [ -d "$ROOT/$DEST_REL" ] || { echo "missing $DEST_REL" >&2; return 1; }
  if diff -r "$tmp" "$ROOT/$DEST_REL" >/dev/null; then
    echo "ok: $DEST_REL is byte-identical to $SRC_PATH at $hash"
  else
    diff -rq "$tmp" "$ROOT/$DEST_REL" >&2 || true
    echo "FAIL: $DEST_REL differs from $SRC_PATH at $hash" >&2
    return 1
  fi
}

self_test() {
  local t aegis mesh
  t="$(mktemp -d)"
  trap 'rm -rf "$t"' RETURN
  aegis="$t/aegis"
  mesh="$t/mesh"
  mkdir -p "$aegis/$SRC_PATH/src" "$mesh/scripts" "$mesh/vendor"
  printf '[package]\nname = "x"\n' > "$aegis/$SRC_PATH/Cargo.toml"
  printf 'line one\r\nline two\r\n' > "$aegis/$SRC_PATH/src/crlf.rs"
  printf '\000\001\002\377' > "$aegis/$SRC_PATH/src/bin.dat"
  printf 'outside\n' > "$aegis/other.txt"
  git -C "$aegis" init -q
  git -C "$aegis" -c core.autocrlf=false add -A
  git -C "$aegis" -c user.name=t -c user.email=t@t -c commit.gpgsign=false commit -q -m one
  cp "${BASH_SOURCE[0]}" "$mesh/scripts/sync-aegis-core.sh"
  local s="$mesh/scripts/sync-aegis-core.sh"

  bash "$s" "$aegis" >/dev/null || { echo "self-test: copy failed" >&2; return 1; }
  bash "$s" "$aegis" --check >/dev/null || { echo "self-test: --check failed on a fresh copy" >&2; return 1; }
  bash "$s" --check >/dev/null || { echo "self-test: bare --check failed" >&2; return 1; }
  cmp -s "$aegis/$SRC_PATH/src/crlf.rs" "$mesh/$DEST_REL/src/crlf.rs" || { echo "self-test: CRLF file altered" >&2; return 1; }
  [ ! -e "$mesh/$DEST_REL/other.txt" ] || { echo "self-test: copied files outside the core" >&2; return 1; }

  printf 'x' >> "$mesh/$DEST_REL/src/crlf.rs"
  if bash "$s" "$aegis" --check >/dev/null 2>&1; then echo "self-test: tampered file passed" >&2; return 1; fi
  bash "$s" "$aegis" >/dev/null
  bash "$s" "$aegis" --check >/dev/null || { echo "self-test: re-copy did not repair" >&2; return 1; }
  : > "$mesh/$DEST_REL/extra.rs"
  if bash "$s" "$aegis" --check >/dev/null 2>&1; then echo "self-test: extra file passed" >&2; return 1; fi
  bash "$s" "$aegis" >/dev/null
  [ ! -e "$mesh/$DEST_REL/extra.rs" ] || { echo "self-test: re-copy kept an extra file" >&2; return 1; }

  printf '# nothing\n' > "$mesh/$NOTE_REL"
  if bash "$s" --check >/dev/null 2>&1; then echo "self-test: hash-less note passed" >&2; return 1; fi
  echo "self-test: ok"
}

case "${1:-}" in
  --self-test) [ $# -eq 1 ] || usage; self_test ;;
  --check) [ $# -eq 1 ] || usage; AEGIS=""; do_check ;;
  ""|-h|--help) usage ;;
  -*) usage ;;
  *)
    AEGIS="$1"
    case "${2:-}" in
      "") do_copy ;;
      --check) [ $# -eq 2 ] || usage; do_check ;;
      *) usage ;;
    esac
    ;;
esac
