#!/usr/bin/env bash
# The app-count sweep for one side: place CPUs (reserve.sh), mount the steady
# disk (disk.sh, RAM-backed ext4 + dm-delay), run step.sh at each app count
# until a step breaks, and always give the CPUs and the disk back.
#
#   bench/scale/sweep.sh rails|rust|rust-memory|rust-mem-ingest [BUSY_EVERY]
#   COUNTS="10 25 50" DURATION=600 bench/scale/sweep.sh rust
#
# Run it (and runall.sh) as a transient unit, so it can be stopped without
# signalling anything by hand:
#   sudo systemd-run --unit=rw-scale-<name> --uid=$(id -u) --gid=$(id -g) ... bench/scale/runall.sh
#   sudo systemctl stop rw-scale-<name>     # stop: SIGTERM to that unit only; the trap below cleans up
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
side=$1 busy=${2:-0}
COUNTS=${COUNTS:-10 25 50 100 250 500}
cleanup() {
  # containers this harness names; never anyone else's
  for c in rvr-rust rvr-rails rvr-rails-jobs; do sudo docker rm -f "$c" >/dev/null 2>&1 || true; done
  "$HERE/disk.sh" down || true
  "$HERE/reserve.sh" off
}
trap cleanup EXIT
trap 'exit 143' TERM INT
"$HERE/reserve.sh" on
"$HERE/disk.sh" up
for n in $COUNTS; do
  line=$("$HERE/step.sh" "$side" "$n" "$busy" | tail -1) || { echo "!! step failed: $side $n" >&2; break; }
  echo "$line"
  echo "scratch: $(( $("$HERE/disk.sh" usage) / 1048576 )) MiB held by the bench disk" >&2
  if ! python3 -I -c 'import json,sys; sys.exit(1 if json.loads(sys.argv[1])["breaks"] else 0)' "$line"; then
    echo "== $side broke at $n apps" >&2
    break
  fi
done
