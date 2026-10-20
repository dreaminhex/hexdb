#!/bin/sh
# Starts HexDB in the container image.
#
# As root (the default) it makes sure the data directory belongs to the hexdb user,
# because a volume can start out owned by root (Fly.io's do), then drops to hexdb
# with setpriv: no extra process, so signals reach hexdb_api directly. Started
# with --user, it runs hexdb_api as that user unchanged.
#
# On Fly.io, other hexes reach this one at <machine>.vm.<app>.internal, so that
# is what it announces unless HEXDB_NETWORK__ADVERTISE_HOST says otherwise.
set -e

if [ -n "$FLY_MACHINE_ID" ] && [ -n "$FLY_APP_NAME" ] && [ -z "$HEXDB_NETWORK__ADVERTISE_HOST" ]; then
  export HEXDB_NETWORK__ADVERTISE_HOST="$FLY_MACHINE_ID.vm.$FLY_APP_NAME.internal"
fi

if [ "$(id -u)" = "0" ]; then
  DATA="${HEXDB_STORAGE__PATH:-/var/lib/hexdb}"
  mkdir -p "$DATA"
  if [ "$(stat -c %U "$DATA")" != "hexdb" ]; then
    chown -R hexdb:hexdb "$DATA"
  fi
  exec setpriv --reuid=hexdb --regid=hexdb --init-groups -- hexdb_api "$@"
fi
exec hexdb_api "$@"
