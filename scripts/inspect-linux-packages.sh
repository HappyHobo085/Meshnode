#!/usr/bin/env bash
# Checks a built meshnode rpm and deb (spec §3.2) without installing them:
#   - both carry /usr/bin/meshd, /usr/lib/systemd/system/meshnode.service (mode 0644) and the
#     package marker /usr/share/meshnode/packaged
#   - both are named meshnode with the version scripts/check-version.sh prints
#   - both require glibc >= 2.39 (deb `libc6 (>= 2.39)`, rpm `libc.so.6(GLIBC_2.39)(64bit)`)
#   - the unit inside each is byte-identical to the committed one
#   - all four scriptlets are present, and shellcheck passes on the scriptlet sources
# Needs rpm on the host; dpkg-deb and shellcheck run in rootless podman
# (docker.io/library/debian:12, docker.io/koalaman/shellcheck:stable).
#
#   inspect-linux-packages.sh <meshnode.rpm> <meshnode.deb>
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PKG_DIR="$REPO/apps/desktop/src-tauri/packaging/linux"
DEBIAN_IMAGE=docker.io/library/debian:12
SHELLCHECK_IMAGE=docker.io/koalaman/shellcheck:stable

[ "$#" -eq 2 ] || { sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }
rpm_file="$(realpath "$1")"
deb_file="$(realpath "$2")"
version="$("$REPO/scripts/check-version.sh")"

fails=0
pass() { echo "ok   $*"; }
fail() { echo "FAIL $*"; fails=$((fails + 1)); }
expect() { # expect <description> <haystack> <needle-regex>
  if grep -Eq -- "$3" <<<"$2"; then pass "$1"; else fail "$1"; fi
}

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "== rpm: $rpm_file"
rpm_list="$(rpm -qlpv "$rpm_file")"
echo "$rpm_list"
expect "rpm has /usr/bin/meshd (executable)" "$rpm_list" '^-rwxr-xr-x .* /usr/bin/meshd$'
expect "rpm has the unit (0644)" "$rpm_list" '^-rw-r--r-- .* /usr/lib/systemd/system/meshnode\.service$'
expect "rpm has the marker" "$rpm_list" ' /usr/share/meshnode/packaged$'
rpm_name="$(rpm -qp --queryformat '%{NAME}' "$rpm_file")"
rpm_version="$(rpm -qp --queryformat '%{VERSION}' "$rpm_file")"
if [ "$rpm_name" = meshnode ]; then pass "rpm name meshnode"; else fail "rpm name is '$rpm_name'"; fi
if [ "$rpm_version" = "$version" ]; then
  pass "rpm version $version"
else
  fail "rpm version '$rpm_version' != '$version'"
fi
expect "rpm requires glibc >= 2.39" "$(rpm -qp --requires "$rpm_file")" '^libc\.so\.6\(GLIBC_2\.39\)\(64bit\)$'
rpm_scripts="$(rpm -qp --scripts "$rpm_file")"
echo "$rpm_scripts" | grep -E '^(pre|post)(install|uninstall) scriptlet' || true
for s in preinstall postinstall preuninstall postuninstall; do
  expect "rpm has a $s scriptlet" "$rpm_scripts" "^$s scriptlet"
done
(cd "$tmp" && mkdir rpm && cd rpm && rpm2cpio "$rpm_file" | cpio -id --quiet ./usr/lib/systemd/system/meshnode.service)
if cmp -s "$tmp/rpm/usr/lib/systemd/system/meshnode.service" "$PKG_DIR/meshnode.service"; then
  pass "rpm unit matches the committed unit"
else
  fail "rpm unit differs from the committed unit"
fi

echo "== deb: $deb_file"
cp "$deb_file" "$tmp/pkg.deb"
podman run --rm --network none -v "$tmp:/w:Z" "$DEBIAN_IMAGE" sh -euc '
  dpkg-deb -c /w/pkg.deb >/w/deb-list
  dpkg-deb -I /w/pkg.deb >/w/deb-info
  dpkg-deb -f /w/pkg.deb Package Version Depends >/w/deb-fields
  mkdir /w/deb-data /w/deb-control
  dpkg-deb -x /w/pkg.deb /w/deb-data
  dpkg-deb -e /w/pkg.deb /w/deb-control
  chmod -R a+rwX /w'
deb_list="$(cat "$tmp/deb-list")"
echo "$deb_list"
cat "$tmp/deb-info"
expect "deb has /usr/bin/meshd (executable)" "$deb_list" '^-rwxr-xr-x .* (\./)?usr/bin/meshd$'
expect "deb has the unit (0644)" "$deb_list" '^-rw-r--r-- .* (\./)?usr/lib/systemd/system/meshnode\.service$'
expect "deb has the marker" "$deb_list" ' (\./)?usr/share/meshnode/packaged$'
deb_fields="$(cat "$tmp/deb-fields")"
expect "deb name meshnode" "$deb_fields" '^Package: meshnode$'
expect "deb version $version" "$deb_fields" "^Version: ${version//./\\.}\$"
expect "deb depends on libc6 (>= 2.39)" "$deb_fields" '^Depends: (.*, )?libc6 \(>= 2\.39\)(,|$)'
for s in preinst postinst prerm postrm; do
  if [ -x "$tmp/deb-control/$s" ]; then pass "deb has an executable $s"; else fail "deb lacks $s"; fi
done
if cmp -s "$tmp/deb-data/usr/lib/systemd/system/meshnode.service" "$PKG_DIR/meshnode.service"; then
  pass "deb unit matches the committed unit"
else
  fail "deb unit differs from the committed unit"
fi

echo "== shellcheck"
# Copied off the checkout: SELinux relabelling (:Z) is unsupported on some filesystems.
cp -R "$PKG_DIR/scripts" "$tmp/scripts"
if podman run --rm --network none -v "$tmp/scripts:/s:ro,Z" "$SHELLCHECK_IMAGE" \
  --shell=sh /s/preinst.sh /s/postinst.sh /s/prerm.sh /s/postrm.sh; then
  pass "shellcheck (sh) on the scriptlets"
else
  fail "shellcheck on the scriptlets"
fi

if [ "$fails" -ne 0 ]; then
  echo "$fails check(s) failed" >&2
  exit 1
fi
echo "all checks passed"
