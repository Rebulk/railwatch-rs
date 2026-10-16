#!/usr/bin/env bash
# Builds a multi-"day" tenant with the stock Rust server + rw-load.
# rw-load stamps every record of a run near the moment the run's batches were
# built, so N runs give N time-ordered clusters ("days"); occurred_at correlates
# with id across runs the way it does in production.
#   build_days.sh DIR DAYS BATCHES_PER_DAY
set -euo pipefail
RS=${RS:-$(cd "$(dirname "$0")/../.." && pwd)}
D=$1 DAYS=$2 PER=$3
mkdir -p "$D"
export RAILWATCH_DATA_DIR=$D RAILWATCH_TRUST_PROXY=1 RAILWATCH_RATE_LIMIT_ENV=0 RAILWATCH_RATE_LIMIT_IP=0 PORT=${PORT:-8801}
[ -f "$D/tokens" ] || $RS/target/release/railwatch env create big | grep -o 'rw_[A-Za-z0-9]*' > "$D/tokens"
taskset -c 8-11 $RS/target/release/railwatch serve > "$D/server.log" 2>&1 &
pid=$!; trap 'kill $pid 2>/dev/null || true' EXIT
until curl -sf "127.0.0.1:$PORT/up" >/dev/null; do sleep 0.1; done
for d in $(seq 1 "$DAYS"); do
  date +%s.%N > "$D/day$d.start"
  taskset -c 12-15 $RS/target/release/rw-load --url "http://127.0.0.1:$PORT" --tokens "$D/tokens" --concurrency 8 \
     --duration 3600 --batches "$PER" --seed "$d" 2>&1 | grep -E "built|batches/s|^ +[0-9]{3} |error" | sed "s/^/day$d /"
done
kill -TERM $pid; wait $pid || true
ls -la "$D/telemetry"
