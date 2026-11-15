#!/usr/bin/env bash
# Every sweep, one after another (never two servers at once). Meant to run as
# a transient unit:
#
#   sudo systemd-run --unit=rw-scale-all --uid=$(id -u) --gid=$(id -g) \
#     --setenv=HOME=$HOME --property=KillMode=mixed --property=TimeoutStopSec=180 \
#     --working-directory=$PWD bench/scale/runall.sh
#   journalctl -u rw-scale-all -f       # progress
#   sudo systemctl stop rw-scale-all    # stop; each sweep's trap cleans up
#
# On stop, systemd (KillMode=mixed) sends SIGTERM to this script only. It
# forwards it to the running sweep -- a child it started, by the PID it
# recorded -- and waits for that sweep's EXIT trap (containers, disk, CPU
# placement), within TimeoutStopSec. A lock serializes every runner on the box.
#
# SWEEPS: "side busy_every [counts...]" entries separated by ';'.
set -euo pipefail
cd "$(dirname "$0")/../.."
# one runner at a time per box: held until this script, cleanup included, exits
exec 9>/var/tmp/rw-scale.lock
flock -n 9 || { echo "another rw-scale runner holds /var/tmp/rw-scale.lock" >&2; exit 75; }
child=
forward() { [ -n "$child" ] && kill -TERM "$child" 2>/dev/null; wait "$child" 2>/dev/null; exit 143; }
trap forward TERM INT
SWEEPS=${SWEEPS:-"rust 0; rust-memory 0; rails 0; rust 10; rails 10"}
IFS=';' read -ra list <<< "$SWEEPS"
for spec in "${list[@]}"; do
  read -ra a <<< "$spec"
  side=${a[0]} busy=${a[1]:-0}
  counts=${a[*]:2}
  echo "#### sweep $side busy_every=$busy counts=${counts:-default} start $(date +%T)"
  rc=0
  if [ -n "$counts" ]; then COUNTS="$counts" bench/scale/sweep.sh "$side" "$busy" & else bench/scale/sweep.sh "$side" "$busy" & fi
  child=$!
  wait "$child" || rc=$?
  child=
  echo "#### sweep $side busy_every=$busy end $(date +%T) rc=$rc"
  [ $rc = 143 ] && exit 143
done
echo "#### ALL DONE $(date +%T)"
