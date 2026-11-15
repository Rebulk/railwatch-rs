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
    # RAILS_BEST_COUNTS: app counts for this sweep (default: the standard set)
    rc=0; COUNTS=${RAILS_BEST_COUNTS:-${COUNTS:-10 25 50 100 250 500}} bench/scale/sweep.sh rails-best 0 & child=$!; wait "$child" || rc=$?; child=
    echo "#### sweep rails-best end $(date +%T) rc=$rc"
    [ $rc = 143 ] && exit 143
  else
    echo "#### rails-best SKIPPED: image ${RAILS_BEST_IMAGE:-unset} not on this host"
  fi
else
  echo "#### rails-best SKIPPED: $CONF not written yet"
fi

# RUST_NEXT_SHA256: the hash the binary's owner gave for it. When set, the
# sweep runs only on that exact binary (it may be replaced by a newer build up
# to the moment this check runs; the hash actually run is recorded either way).
if [ -x "$BIN/railwatch-next" ]; then
  have=$(sha256sum "$BIN/railwatch-next" | cut -d' ' -f1)
  echo "#### rust-next binary sha256=$have ($(stat -c %y "$BIN/railwatch-next" | cut -c1-19))"
  if [ -n "${RUST_NEXT_SHA256:-}" ] && [ "$have" != "$RUST_NEXT_SHA256" ]; then
    echo "#### rust-next SKIPPED: sha256 $have is not the expected $RUST_NEXT_SHA256"
    echo "#### FOLLOWUP DONE $(date +%T)"; exit 0
  fi
  echo "#### sweep rust-next sha256=$have start $(date +%T)"
  rc=0; bench/scale/sweep.sh rust-next 0 & child=$!; wait "$child" || rc=$?; child=
  echo "#### sweep rust-next end $(date +%T) rc=$rc"
  [ $rc = 143 ] && exit 143
else
  echo "#### rust-next SKIPPED: $BIN/railwatch-next not there yet"
fi
echo "#### FOLLOWUP DONE $(date +%T)"
