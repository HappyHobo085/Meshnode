#!/bin/sh
# meshnode postrm (deb postrm, rpm %postun); spec §3.2. One script serves both formats:
#   rpm: $1 = 0 remove, 1+ upgrade
#   deb: $1 = remove | purge | upgrade new-version | disappear | failed-upgrade | abort-* ...
# Always reloads systemd. Only deb `purge` deletes /var/lib/meshnode and the meshnode
# user/group; rpm has no purge (the checklist gives the manual commands).
set -e

NAME=meshnode
DATA=/var/lib/meshnode

if command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]; then
  systemctl daemon-reload || true
fi

if [ "$1" != purge ]; then
  exit 0
fi

rm -rf "$DATA"
if getent passwd "$NAME" >/dev/null; then
  if command -v userdel >/dev/null 2>&1; then
    userdel "$NAME" || true
  elif command -v deluser >/dev/null 2>&1; then
    deluser --system "$NAME" || true
  fi
fi
if getent group "$NAME" >/dev/null; then
  if command -v groupdel >/dev/null 2>&1; then
    groupdel "$NAME" || true
  elif command -v delgroup >/dev/null 2>&1; then
    delgroup --system "$NAME" || true
  fi
fi

exit 0
