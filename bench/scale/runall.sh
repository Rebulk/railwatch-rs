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
# SWEEPS: "side busy_every [counts...]" entries separated by ';'.
set -euo pipefail
cd "$(dirname "$0")/../.."
SWEEPS=${SWEEPS:-"rust 0; rust-memory 0; rails 0; rust 10; rails 10"}
IFS=';' read -ra list <<< "$SWEEPS"
for spec in "${list[@]}"; do
  read -ra a <<< "$spec"
  side=${a[0]} busy=${a[1]:-0}
  counts=${a[*]:2}
  echo "#### sweep $side busy_every=$busy counts=${counts:-default} start $(date +%T)"
  rc=0
  if [ -n "$counts" ]; then COUNTS="$counts" bench/scale/sweep.sh "$side" "$busy" || rc=$?
  else bench/scale/sweep.sh "$side" "$busy" || rc=$?; fi
  echo "#### sweep $side busy_every=$busy end $(date +%T) rc=$rc"
  [ $rc = 143 ] && exit 143
done
echo "#### ALL DONE $(date +%T)"
