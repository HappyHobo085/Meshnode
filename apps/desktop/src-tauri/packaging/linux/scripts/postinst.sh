#!/bin/sh
# meshnode postinst (deb postinst, rpm %post); spec §3.2. One script serves both formats:
#   rpm: $1 = 1 install, 2+ upgrade
#   deb: $1 = configure [old-version] | abort-upgrade | abort-remove | abort-deconfigure ...
# First install: enable and start meshnode.service. Upgrade: restart it if it is running
# (the unit stays as enabled or disabled as it was).
set -e

UNIT=meshnode.service
# Written by preinst when dpkg installs rather than upgrades (see preinst.sh).
INSTALL_FLAG=/run/meshnode-package-install

case "$1" in
  1) action=install ;;
  [0-9]*) action=upgrade ;;
  configure)
    if [ -z "$2" ] || [ -e "$INSTALL_FLAG" ]; then
      action=install
    else
      action=upgrade
    fi
    ;;
  # prerm `remove` stopped and disabled the unit; a failed removal puts it back.
  abort-remove | abort-deconfigure) action=install ;;
  abort-upgrade) action=upgrade ;;
  *) action=other ;;
esac
rm -f "$INSTALL_FLAG"

if [ "$action" = other ] || ! command -v systemctl >/dev/null 2>&1; then
  exit 0
fi

if [ -d /run/systemd/system ]; then
  systemctl daemon-reload || true
  if [ "$action" = install ]; then
    systemctl enable --now "$UNIT"
  else
    systemctl try-restart "$UNIT" || true
  fi
elif [ "$action" = install ]; then
  # systemd is not running (a container or chroot): enable for the next boot only.
  systemctl enable "$UNIT" || true
fi

exit 0
