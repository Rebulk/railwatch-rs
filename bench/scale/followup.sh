#!/usr/bin/env bash
# The follow-up sweeps: rails-best (Track G's image) and rust-next (Track H's
# binary), after the main queue (unit rw-scale-all) has finished. Each runs
# only once its artifact is present; a missing one is reported, not guessed.
#
#   sudo systemd-run --unit=rw-scale-followup --uid=$(id -u) --gid=$(id -g) \
#     --setenv=HOME=$HOME --property=KillMode=mixed --property=TimeoutStopSec=180 \
#     --working-directory=$PWD bench/scale/followup.sh
#
# Inputs (files, so nothing secret is passed on a command line):
#   $BIN/railwatch-next              Track H's binary
#   ~/.config/rw-scale/rails-best    RAILS_BEST_IMAGE=<tag>, and optionally
#                                    RAILS_EXTRA_ENV=<path to a non-secret --env-file>
set -euo pipefail
cd "$(dirname "$0")/../.."
BIN=${BIN:-$HOME/.cache/rw-scale/bin}
CONF=${RAILS_BEST_CONF:-$HOME/.config/rw-scale/rails-best}

# never two servers at once: runall.sh holds this lock until it and its
# cleanup have exited (a unit still "deactivating" still holds it)
exec 9>/var/tmp/rw-scale.lock
flock 9
echo "#### followup start $(date +%T) (lock acquired: no other runner)"
child=
forward() { [ -n "$child" ] && kill -TERM "$child" 2>/dev/null; wait "$child" 2>/dev/null; exit 143; }
trap forward TERM INT

if [ -f "$CONF" ]; then
  # shellcheck disable=SC1090
  . "$CONF"
  export RAILS_BEST_IMAGE RAILS_EXTRA_ENV
  if sudo docker image inspect "${RAILS_BEST_IMAGE:-none}" >/dev/null 2>&1; then
    echo "#### sweep rails-best image=$RAILS_BEST_IMAGE start $(date +%T)"
    rc=0; bench/scale/sweep.sh rails-best 0 & child=$!; wait "$child" || rc=$?; child=
    echo "#### sweep rails-best end $(date +%T) rc=$rc"
    [ $rc = 143 ] && exit 143
  else
    echo "#### rails-best SKIPPED: image ${RAILS_BEST_IMAGE:-unset} not on this host"
  fi
else
  echo "#### rails-best SKIPPED: $CONF not written yet"
fi

if [ -x "$BIN/railwatch-next" ]; then
  echo "#### sweep rust-next sha256=$(sha256sum "$BIN/railwatch-next" | cut -c1-12) start $(date +%T)"
  rc=0; bench/scale/sweep.sh rust-next 0 & child=$!; wait "$child" || rc=$?; child=
  echo "#### sweep rust-next end $(date +%T) rc=$rc"
  [ $rc = 143 ] && exit 143
else
  echo "#### rust-next SKIPPED: $BIN/railwatch-next not there yet"
fi
echo "#### FOLLOWUP DONE $(date +%T)"
