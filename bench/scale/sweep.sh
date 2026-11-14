#!/usr/bin/env bash
# The app-count sweep for one side: reserve CPUs, mount the ext4 disk, run
# step.sh at each app count until a step breaks (then one more count is NOT
# tried), and always give the CPUs and disk back, on failure too.
#
#   bench/scale/sweep.sh rails|rust|rust-memory [BUSY_EVERY]
#   COUNTS="10 25 50" DURATION=600 bench/scale/sweep.sh rust
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
side=$1 busy=${2:-0}
COUNTS=${COUNTS:-10 25 50 100 250 500}
cleanup() {
  sudo docker rm -f rvr-rust rvr-rails rvr-rails-jobs >/dev/null 2>&1 || true
  "$HERE/disk.sh" down || true
  "$HERE/reserve.sh" off
}
trap cleanup EXIT INT TERM
"$HERE/reserve.sh" on
"$HERE/disk.sh" up
for n in $COUNTS; do
  line=$("$HERE/step.sh" "$side" "$n" "$busy" | tail -1) || { echo "!! step failed: $side $n" >&2; break; }
  echo "$line"
  echo "scratch: image $(( $("$HERE/disk.sh" usage) / 1048576 )) MiB on host" >&2
  if ! python3 -I -c 'import json,sys; sys.exit(1 if json.loads(sys.argv[1])["breaks"] else 0)' "$line"; then
    echo "== $side broke at $n apps" >&2
    break
  fi
done
