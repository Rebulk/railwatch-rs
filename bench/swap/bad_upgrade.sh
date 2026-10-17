#!/bin/sh
# A release that fails only once it runs for real, for railwatch-swap's
# rollback path (zero_drop.sh FAIL_AT=): it passes `version`, waits for the
# data dir lock like serve, then exits as soon as it holds it, so the new
# colour never passes the health check while the old colour is down.
[ "${1:-}" = version ] && { echo "railwatch bad-upgrade (bench/swap)"; exit 0; }
echo "railwatch: waiting for /data/serve.lock: bad upgrade" >&2
exec flock /data/serve.lock sh -c 'echo "bad upgrade: exiting" >&2; exit 1'
