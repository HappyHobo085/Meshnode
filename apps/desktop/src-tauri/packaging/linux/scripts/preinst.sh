#!/bin/sh
# meshnode preinst (deb preinst, rpm %pre); spec §3.2. One script serves both formats:
#   rpm: $1 = 1 install, 2+ upgrade
#   deb: $1 = install [old-version] | upgrade old-version | abort-upgrade new-version
# Creates the `meshnode` system user and group the unit runs as, when missing. The account
# and /var/lib/meshnode outlive upgrades and removal; only `dpkg --purge` deletes them.
set -e

NAME=meshnode
DATA=/var/lib/meshnode
# Tells postinst that dpkg is installing (fresh, or again after a remove), not upgrading:
# deb postinst gets `configure <old-version>` in both cases.
INSTALL_FLAG=/run/meshnode-package-install

case "$1" in
  1 | install) action=install ;;
  [0-9]* | upgrade) action=upgrade ;;
  *) action=other ;;
esac

if [ "$1" = install ]; then
  # touch, not `: >file`: a failed redirection on a special builtin exits sh despite `|| true`.
  touch "$INSTALL_FLAG" 2>/dev/null || true
fi

if [ "$action" = other ]; then
  exit 0
fi

if ! getent passwd "$NAME" >/dev/null; then
  if command -v useradd >/dev/null 2>&1; then
    if getent group "$NAME" >/dev/null; then
      group_arg="-g $NAME"
    else
      group_arg=--user-group
    fi
    # $group_arg is two words or one, deliberately split.
    # shellcheck disable=SC2086
    useradd --system --no-create-home --home-dir "$DATA" --shell /usr/sbin/nologin \
      $group_arg "$NAME"
  elif command -v adduser >/dev/null 2>&1; then
    adduser --system --group --no-create-home --home "$DATA" --shell /usr/sbin/nologin "$NAME"
  else
    echo "meshnode: neither useradd nor adduser found; cannot create the $NAME user" >&2
    exit 1
  fi
fi

exit 0
