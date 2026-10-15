#!/bin/sh
# Starts uptimestatus as the unprivileged `app` user. When started as root (as
# on most platforms), first hands the storage volume (/data by default) to that user: a
# fresh volume is owned by root.
set -eu
if [ "$(id -u)" = 0 ]; then
  # Ping monitors need unprivileged ICMP sockets. A VM (Fly Machines) lets
  # root allow them; a container's /proc/sys is usually read-only, so this may
  # fail harmlessly (Docker already allows them). The setting covers ICMPv6 too.
  echo "0 2147483647" > /proc/sys/net/ipv4/ping_group_range 2>/dev/null || true
  if [ -d /data ]; then
    chown app:app /data
  fi
  dir="${UPTIMESTATUS_STORAGE__PATH:-}"
  if [ -n "$dir" ] && [ -d "$dir" ]; then
    chown app:app "$dir"
  fi
  exec setpriv --reuid=app --regid=app --clear-groups /app/uptimestatus "$@"
fi
exec /app/uptimestatus "$@"
