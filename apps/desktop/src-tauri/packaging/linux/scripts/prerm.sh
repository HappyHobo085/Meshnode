#!/bin/sh
# meshnode prerm (deb prerm, rpm %preun); spec §3.2. One script serves both formats:
#   rpm: $1 = 0 remove, 1+ upgrade
#   deb: $1 = remove | upgrade new-version | deconfigure ... | failed-upgrade old-version
# Remove: stop and disable meshnode.service. Upgrade: nothing (postinst restarts it).
set -e

UNIT=meshnode.service

case "$1" in
  0 | remove) action=remove ;;
  *) action=other ;;
esac

if [ "$action" != remove ] || ! command -v systemctl >/dev/null 2>&1; then
  exit 0
fi

if [ -d /run/systemd/system ]; then
  systemctl disable --now "$UNIT" || true
else
  systemctl disable "$UNIT" || true
fi

exit 0
