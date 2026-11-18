#!/usr/bin/env bash
# Container lifecycle test for the meshnode rpm and deb (spec §3.2, §7): in rootless podman,
# with systemd running as PID 1 in each container, it runs
#   install old → upgrade to new → remove → reinstall → purge
# and asserts the meshnode user, /usr/bin/meshd's version, that the unit is enabled and meshd
# runs (no restart loop), that upgrade restarts the service, and that /var/lib/meshnode
# survives upgrade and remove. Purge is `dpkg --purge` for the deb and RPM_MANUAL_PURGE (below)
# for the rpm. The deb run first checks that a failed install (abort-install) leaves no
# install flag behind.
#   rpm: registry.fedoraproject.org/fedora:41    deb: docker.io/library/debian:12
# (override with MESHNODE_TEST_FEDORA_IMAGE / MESHNODE_TEST_DEBIAN_IMAGE)
# Each base image gets systemd and the packages' GUI dependencies (webkit2gtk, gtk3) in a
# local image, built once and tagged by a hash of its Containerfile; that build needs the
# network. The lifecycle containers then run with --network none.
#
#   test-linux-packages.sh <old.rpm> <new.rpm> <old.deb> <new.deb>
#
# "old" is any earlier build, e.g. one made after `scripts/set-version.sh 0.0.1` in a scratch
# copy of the repo. Exits 0 when every assertion passes.
# meshd must run on each image's glibc: a binary linked against a newer glibc than the image's
# fails the version and "meshd runs" checks.
# The single-quoted commands run in the containers' sh, which expands them there.
# shellcheck disable=SC2016
set -euo pipefail

FEDORA_IMAGE="${MESHNODE_TEST_FEDORA_IMAGE:-registry.fedoraproject.org/fedora:41}"
DEBIAN_IMAGE="${MESHNODE_TEST_DEBIAN_IMAGE:-docker.io/library/debian:12}"
# rpm has no purge; docs/e2e-checklist.md gives the owner this exact command.
RPM_MANUAL_PURGE='rpm -e meshnode && userdel meshnode && rm -rf /var/lib/meshnode'

[ "$#" -eq 4 ] || { sed -n '2,21p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }
for f in "$@"; do [ -f "$f" ] || { echo "no such package: $f" >&2; exit 2; }; done
old_rpm="$(realpath "$1")"
new_rpm="$(realpath "$2")"
old_deb="$(realpath "$3")"
new_deb="$(realpath "$4")"

fails=0
pass() { echo "ok   $*"; }
fail() { echo "FAIL $*"; fails=$((fails + 1)); }

containers=()
cleanup() { [ "${#containers[@]}" -eq 0 ] || podman rm -f -t 0 "${containers[@]}" >/dev/null 2>&1 || true; }
trap cleanup EXIT

# build_image <base> <containerfile-body> → prints the local tag
build_image() {
  local base="$1" body="$2" name hash tag ctx
  name="$(basename "${base%%:*}")"
  hash="$(printf '%s' "$body" | sha256sum | cut -c1-12)"
  tag="localhost/meshnode-pkgtest-$name:$hash"
  if ! podman image exists "$tag"; then
    ctx="$(mktemp -d)"
    printf '%s\n' "$body" >"$ctx/Containerfile"
    podman build -q -t "$tag" "$ctx" >&2
    rm -rf "$ctx"
  fi
  echo "$tag"
}

# start <image> <name> → boots systemd in a new container and waits for it
start() {
  local image="$1" name="$2" state
  podman run -d --name "$name" --systemd=always --network none "$image" /sbin/init >/dev/null
  containers+=("$name")
  podman exec "$name" mkdir -p /pkgs
  # is-system-running fails at once until PID 1's bus is up, so ask again until it answers.
  for _ in $(seq 60); do
    state="$(podman exec "$name" timeout 120 systemctl is-system-running --wait 2>/dev/null || true)"
    [ -z "$state" ] || break
    sleep 1
  done
  case "$state" in
    running | degraded) pass "$name: systemd is up ($state)" ;;
    *) fail "$name: systemd did not come up ($state)"; return 1 ;;
  esac
}

# cexec <container> <sh-command>: runs a command in the container, output shown
cexec() { podman exec "$1" sh -c "$2"; }
# q <container> <sh-command>: prints the command's stdout, never fails
q() { podman exec "$1" sh -c "$2" 2>/dev/null || true; }

# step <container> <description> <sh-command>: a lifecycle step; failure ends the run
step() {
  echo "-- $1: $2"
  if cexec "$1" "$3"; then pass "$1: $2"; else fail "$1: $2"; return 1; fi
}

# check <container> <description> <sh-test>
check() {
  if podman exec "$1" sh -c "$3" >/dev/null 2>&1; then pass "$1: $2"; else fail "$1: $2"; fi
}

check_version() { # <container> <version>
  local got
  got="$(q "$1" '/usr/bin/meshd --version')"
  if [ "$got" = "meshd $2" ]; then pass "$1: meshd reports $2"; else fail "$1: meshd reports '$got', want $2"; fi
}

# A unit whose ExecStart keeps failing flips between active and activating (Restart=always),
# so "running" means: running within 10 s, and still the same process 3 s later.
MESHD_RUNS='
  i=0
  until [ "$(systemctl show -p SubState --value meshnode.service)" = running ]; do
    i=$((i + 1)); [ "$i" -le 20 ] || exit 1; sleep 0.5
  done
  pid="$(systemctl show -p MainPID --value meshnode.service)"
  sleep 3
  [ "$(systemctl show -p SubState --value meshnode.service)" = running ] &&
    [ "$(systemctl show -p MainPID --value meshnode.service)" = "$pid" ] &&
    [ "$(systemctl show -p NRestarts --value meshnode.service)" = 0 ]'

check_running() { # <container>: user, unit enabled, meshd running
  check "$1" "user meshnode exists" 'getent passwd meshnode'
  check "$1" "unit enabled" '[ "$(systemctl is-enabled meshnode.service)" = enabled ]'
  check "$1" "meshd runs under the unit" "$MESHD_RUNS"
}

invocation() { q "$1" 'systemctl show -p InvocationID --value meshnode.service'; }

# lifecycle <container> <old-version> <new-version> <install-old> <upgrade> <remove> <reinstall>
lifecycle() {
  local c="$1" old_v="$2" new_v="$3" before after
  step "$c" "install $old_v" "$4" || return 0
  check_version "$c" "$old_v"
  check_running "$c"
  step "$c" "write the sentinel" 'echo sentinel >/var/lib/meshnode/sentinel' || return 0
  before="$(invocation "$c")"

  step "$c" "upgrade to $new_v" "$5" || return 0
  check_version "$c" "$new_v"
  check_running "$c"
  after="$(invocation "$c")"
  if [ -n "$after" ] && [ "$after" != "$before" ]; then
    pass "$c: upgrade restarted the service"
  else
    fail "$c: upgrade did not restart the service (InvocationID '$before' → '$after')"
  fi
  check "$c" "sentinel survives the upgrade" '[ -f /var/lib/meshnode/sentinel ]'

  step "$c" "remove (remove keeps data)" "$6" || return 0
  check "$c" "binary gone after remove" '[ ! -e /usr/bin/meshd ]'
  check "$c" "unit file gone after remove" '[ ! -e /usr/lib/systemd/system/meshnode.service ]'
  check "$c" "service stopped after remove" '! systemctl is-active --quiet meshnode.service'
  check "$c" "sentinel survives remove" '[ -f /var/lib/meshnode/sentinel ]'
  check "$c" "user survives remove" 'getent passwd meshnode'

  step "$c" "reinstall $new_v" "$7" || return 0
  check_version "$c" "$new_v"
  check_running "$c"
  check "$c" "sentinel present after reinstall" '[ -f /var/lib/meshnode/sentinel ]'
}

check_purged() { # <container>
  check "$1" "binary gone after purge" '[ ! -e /usr/bin/meshd ]'
  check "$1" "/var/lib/meshnode gone after purge" '[ ! -e /var/lib/meshnode ]'
  check "$1" "user gone after purge" '! getent passwd meshnode'
  check "$1" "group gone after purge" '! getent group meshnode'
}

run=$$

echo "== rpm ($FEDORA_IMAGE)"
fedora="$(build_image "$FEDORA_IMAGE" "FROM $FEDORA_IMAGE
RUN dnf -y install --setopt=install_weak_deps=False systemd shadow-utils webkit2gtk4.1 gtk3 && dnf clean all")"
c="meshnode-pkgtest-rpm-$run"
if start "$fedora" "$c"; then
  podman cp "$old_rpm" "$c:/pkgs/old.rpm"
  podman cp "$new_rpm" "$c:/pkgs/new.rpm"
  old_v="$(q "$c" 'rpm -qp --nosignature --queryformat %{VERSION} /pkgs/old.rpm')"
  new_v="$(q "$c" 'rpm -qp --nosignature --queryformat %{VERSION} /pkgs/new.rpm')"
  # The packages are unsigned; Fedora's rpm refuses those without --nosignature.
  lifecycle "$c" "$old_v" "$new_v" \
    'rpm -i --nosignature /pkgs/old.rpm' \
    'rpm -U --nosignature /pkgs/new.rpm' \
    'rpm -e meshnode' \
    'rpm -i --nosignature /pkgs/new.rpm'
  if step "$c" "manual purge: $RPM_MANUAL_PURGE" "$RPM_MANUAL_PURGE"; then
    check_purged "$c"
  fi
fi

echo "== deb ($DEBIAN_IMAGE)"
debian="$(build_image "$DEBIAN_IMAGE" "FROM $DEBIAN_IMAGE
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends systemd systemd-sysv libwebkit2gtk-4.1-0 libgtk-3-0 && rm -rf /var/lib/apt/lists/*")"
c="meshnode-pkgtest-deb-$run"
if start "$debian" "$c"; then
  podman cp "$old_deb" "$c:/pkgs/old.deb"
  podman cp "$new_deb" "$c:/pkgs/new.deb"
  old_v="$(q "$c" 'dpkg-deb -f /pkgs/old.deb Version')"
  new_v="$(q "$c" 'dpkg-deb -f /pkgs/new.deb Version')"
  apt='DEBIAN_FRONTEND=noninteractive apt-get -y -q'
  # Another package that owns one of meshnode's files makes dpkg's unpack fail after preinst
  # ran, so dpkg calls `postrm abort-install`, which must clear preinst's install flag.
  step "$c" "install a package that owns /usr/share/meshnode/packaged" '
    mkdir -p /tmp/blocker/DEBIAN /tmp/blocker/usr/share/meshnode
    : >/tmp/blocker/usr/share/meshnode/packaged
    printf "Package: meshnode-blocker\nVersion: 1\nArchitecture: all\nMaintainer: test\nDescription: test\n" \
      >/tmp/blocker/DEBIAN/control
    dpkg-deb -b /tmp/blocker /tmp/blocker.deb >/dev/null && dpkg -i /tmp/blocker.deb >/dev/null' || true
  echo "-- $c: install $old_v over the conflict (expected to fail)"
  if cexec "$c" "$apt install /pkgs/old.deb >/dev/null 2>&1"; then
    fail "$c: install over a file conflict unexpectedly succeeded"
  else
    pass "$c: install over a file conflict fails"
  fi
  check "$c" "abort-install clears the install flag" '[ ! -e /run/meshnode-package-install ]'
  check "$c" "meshnode is not installed after the failed install" \
    '[ "$(dpkg-query -W -f "\${db:Status-Status}" meshnode 2>/dev/null)" != installed ]'
  step "$c" "remove the conflicting package" 'dpkg --purge meshnode-blocker >/dev/null' || true

  lifecycle "$c" "$old_v" "$new_v" \
    "$apt install /pkgs/old.deb" \
    "$apt install /pkgs/new.deb" \
    "$apt remove meshnode" \
    "$apt install /pkgs/new.deb"
  if step "$c" "purge (purge deletes data)" 'dpkg --purge meshnode'; then
    check_purged "$c"
  fi
fi

if [ "$fails" -ne 0 ]; then
  echo "$fails check(s) failed" >&2
  exit 1
fi
echo "all checks passed"
