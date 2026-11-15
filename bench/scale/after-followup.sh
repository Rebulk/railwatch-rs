#!/usr/bin/env bash
# After the current follow-up (rust-next) has finished and cleaned up:
#   1. thp_ab.sh: the same binary with THP as the host has it and with THP off
#      (memory only), on the bench's own CPUs with nothing else running;
#   2. the rails-best sweep (RAILS_BEST_COUNTS).
# Waits on the same lock every runner holds through its cleanup, so it never
# overlaps a sweep. Run it as a unit:
#   sudo systemd-run --unit=rw-scale-final --uid=$(id -u) --gid=$(id -g) --setenv=HOME=$HOME \
#     --property=KillMode=mixed --property=TimeoutStopSec=180 --working-directory=$PWD bench/scale/after-followup.sh
set -euo pipefail
cd "$(dirname "$0")/../.."
exec 9>/var/tmp/rw-scale.lock
flock 9
# the lock is free once the follow-up's sweep and its cleanup are done; check it
mountpoint -q /var/tmp/rw-scale/mnt && { echo "after-followup: bench mount still up" >&2; exit 1; }
[ -z "$(sudo docker ps -aq --filter name=rvr-)" ] || { echo "after-followup: rvr- containers left" >&2; exit 1; }
echo "#### after-followup start $(date +%T) (lock acquired, bench clean)"
child=
forward() { [ -n "$child" ] && kill -TERM "$child" 2>/dev/null; wait "$child" 2>/dev/null; exit 143; }
trap forward TERM INT

# 1. THP A/B: inside the bench slices, on the bench's CPUs
bench/scale/reserve.sh on >/dev/null
rc=0
for spec in ${THP_SPECS:-main:25 main:100}; do
  bin=${spec%%:*} apps=${spec##*:}
  echo "#### thp_ab $bin $apps apps start $(date +%T)"
  sudo systemd-run --scope --quiet --slice=bench-server.slice --uid="$(id -u)" --gid="$(id -g)" -- \
    bash -c "SERVER_CPUS=11,23 LOAD_CPUS=6-10,18-22 THP_SKIP_BUSY_CHECK=1 bench/scale/thp_ab.sh \"\$HOME/.cache/rw-scale/bin/railwatch-$bin\" $apps ${THP_SECS:-180}" \
    | tee -a bench/results/scale/thp_ab.jsonl & child=$!
  wait "$child" || rc=$?; child=
  echo "#### thp_ab $bin $apps apps end $(date +%T) rc=$rc"
done
bench/scale/reserve.sh off >/dev/null

# 2. rails-best
CONF=${RAILS_BEST_CONF:-$HOME/.config/rw-scale/rails-best}
# shellcheck disable=SC1090
. "$CONF"
export RAILS_BEST_IMAGE RAILS_EXTRA_ENV
sudo docker image inspect "$RAILS_BEST_IMAGE" >/dev/null
echo "#### sweep rails-best image=$RAILS_BEST_IMAGE id=$(sudo docker image inspect -f '{{.Id}}' "$RAILS_BEST_IMAGE" | cut -c8-19) counts=${RAILS_BEST_COUNTS:-default} start $(date +%T)"
rc=0; COUNTS=${RAILS_BEST_COUNTS:-10 25 50 100 150 250 350 500} bench/scale/sweep.sh rails-best 0 & child=$!; wait "$child" || rc=$?; child=
echo "#### sweep rails-best end $(date +%T) rc=$rc"
echo "#### AFTER-FOLLOWUP DONE $(date +%T)"
