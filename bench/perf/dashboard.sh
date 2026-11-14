#!/usr/bin/env bash
# Cold and warm timings of the main dashboard pages and MCP tools through
# the real router (page-replay), over one busy per-day tenant
# (docs/PERF.md, "Cold dashboard reads").
#
#   NOW=EPOCH bench/perf/dashboard.sh ingest BATCH_DIR... DATA   # fresh server, post, settle
#   bench/perf/dashboard.sh run DATA LABEL [BIN_DIR=target/release] [ROUNDS=3] [WARM=5]
#   bench/perf/dashboard.sh report RUNS_DIR LABEL... (before/after table)
#
# ingest: `railwatch setup` in DATA, `serve` on CPUs 8-11, every batch of each
# BATCH_DIR posted in order (post.py), 90 s for the rollups to flush, stop,
# then every hour's rollups recomputed from the rows (rw-rollup over the
# per-day directory): on a loaded box the server's flushes can fail on
# "database is locked" and leave hours short. DATA/now is NOW (the
# generators' --now), DATA/cases.json the cases (cases.py).
#
# run: one copy of DATA per run (cp: new files no process has open), then
# per round and per case a COLD call: the copy's files dropped from the
# page cache (evict.py: fdatasync + posix_fadvise DONTNEED, which needs no
# root; the MiB still resident go to evict.log), a fresh page-replay
# process, that one case as its first call. On a loop-mounted image, set
# EVICT_ALSO to the image file so its own cached pages go too.
# Then a WARM pass: one process, every case 1 + WARM times on the
# original DATA (already in the page cache), the first call dropped.
# Writes
# DATA/../runs/LABEL/{cold-R-case,warm-R}.json(.timings.json).
set -euo pipefail
umask 077
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
HERE=$ROOT/bench/perf
cmd=${1:?usage: dashboard.sh ingest|run|report ...}
shift
case $cmd in
ingest)
  D=${*: -1}
  BIN=${BIN_DIR:-$ROOT/target/release}
  [ ! -e "$D" ] || { echo "$D exists" >&2; exit 1; }
  mkdir -p "$D"
  PORT=${PORT:-8797}
  export RAILWATCH_DATA_DIR=$D RAILWATCH_RATE_LIMIT_ENV=0 RAILWATCH_RATE_LIMIT_IP=0 RAILWATCH_BIND=127.0.0.1:$PORT RAILWATCH_URL=http://127.0.0.1:$PORT
  pw=$(python3 -I -c 'import secrets; print(secrets.token_urlsafe(18))')
  echo "$pw" | "$BIN/railwatch" setup --email bench@example.com --password-stdin --app bench --env production | sed -n 's/.*RAILWATCH_TOKEN=//p' > "$D/token"
  taskset -c "${SERVER_CPUS:-8-11}" "$BIN/railwatch" serve > "$D/server.log" 2>&1 &
  pid=$!
  until curl -sf "127.0.0.1:$PORT/up" > /dev/null; do sleep 0.2; done
  s=$(date +%s)
  for B in "${@:1:$#-1}"; do
    taskset -c "${POST_CPUS:-12-13}" python3 -I "$ROOT/bench/diff/post.py" "http://127.0.0.1:$PORT/ingest" "$(cat "$D/token")" "$B" "$D/acks-$(basename "$B").json"
    curl -s -X POST -H "Authorization: Bearer $(cat "$D/token")" -H 'Content-Type: application/json' --data-binary @"$B/deploys.json" "http://127.0.0.1:$PORT/ingest/deploys" > /dev/null || true
  done
  echo "wall $(( $(date +%s) - s )) s, server cpu_ticks $(awk '{print $14+$15}' /proc/$pid/stat)" | tee "$D/ingest.txt"
  sleep 90
  kill "$pid"; wait "$pid" 2> /dev/null || true
  for T in "$D"/telemetry/*/; do "$BIN/rw-rollup" "${T%/}" all 2> /dev/null; done
  echo "${NOW:?NOW: the --now the batches were generated with}" > "$D/now"
  python3 -I "$HERE/cases.py" "$D" "$D/cases.json"
  du -sh "$D/telemetry"
  ;;
run)
  D=${1:?DATA} L=${2:?LABEL} BIN=${3:-$ROOT/target/release} ROUNDS=${4:-3} WARM=${5:-5}
  NOW=$(cat "$D/now") USER_ID=$(cat "$D/cases.json.user")
  OUT=$(dirname "$D")/runs/$L COLD=$(dirname "$D")/cold
  mkdir -p "$OUT"
  rm -rf "$COLD"; cp -a "$D" "$COLD"
  names=$(python3 -I -c 'import json,sys; [print(c["name"]) for c in json.load(open(sys.argv[1]))]' "$D/cases.json")
  for r in $(seq 1 "$ROUNDS"); do
    for n in $names; do
      python3 -I -c 'import json,sys; json.dump([c for c in json.load(open(sys.argv[1])) if c["name"] == sys.argv[2]], open(sys.argv[3], "w"))' "$D/cases.json" "$n" "$COLD/one.json"
      python3 -I "$HERE/evict.py" "$COLD" ${EVICT_ALSO:-} >> "$OUT/evict.log"
      taskset -c "${CPUS:-8-11}" "$BIN/page-replay" "$COLD" "$COLD/one.json" "$NOW" "$USER_ID" "$OUT/cold-$r-$n.json" 0 2> /dev/null
    done
    taskset -c "${CPUS:-8-11}" "$BIN/page-replay" "$D" "$D/cases.json" "$NOW" "$USER_ID" "$OUT/warm-$r.json" "$WARM" 2> /dev/null
  done
  rm -rf "$COLD"
  ;;
report)
  python3 -I "$HERE/report.py" "$@"
  ;;
*)
  echo "usage: dashboard.sh ingest|run|report ..." >&2
  exit 2
  ;;
esac
