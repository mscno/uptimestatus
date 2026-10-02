#!/bin/sh
# Starts uptimestatus as the unprivileged `app` user. When started as root (as
# on most platforms), first hands the storage volume (/data by default) to that user: a
# fresh volume is owned by root.
set -eu
if [ "$(id -u)" = 0 ]; then
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
