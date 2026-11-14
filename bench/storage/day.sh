#!/usr/bin/env bash
# Ingests one prod-shaped day into a fresh per-day environment and prints
# bytes per table, per index and per event (bench/storage/bytes.py).
#
#   bench/storage/day.sh BATCH_DIR [label]
#
# BATCH_DIR is gen_tenant.py output (docs/STORAGE.md, "Sizing", has the
# command). BIN_DIR (default target/release), PORT (8811), CPUS (8-11).
# The server is stopped before measuring, so every WAL is checkpointed.
# KEEP=1 keeps the data dir (printed).
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
SRC=${1:?usage: day.sh BATCH_DIR [label]} LABEL=${2:-day}
BIN=${BIN_DIR:-$ROOT/target/release} PORT=${PORT:-8811}
mkdir -p /dev/shm/rwps
D=$(mktemp -d /dev/shm/rwps/$LABEL.XXXX)
export RAILWATCH_DATA_DIR=$D RAILWATCH_RATE_LIMIT_ENV=0 PORT
tok=$("$BIN/railwatch" env create e1 | grep -o 'rw_[A-Za-z0-9]*')
taskset -c "${CPUS:-8-11}" "$BIN/railwatch" serve > "$D/server.log" 2>&1 &
pid=$!
trap 'kill $pid 2>/dev/null || true; wait $pid 2>/dev/null || true; [ -n "${KEEP:-}" ] || rm -rf "$D"' EXIT
until curl -sf "127.0.0.1:$PORT/up" >/dev/null; do sleep 0.1; done
t0=$(date +%s.%N)
python3 -I "$ROOT/bench/diff/post.py" "http://127.0.0.1:$PORT/ingest" "$tok" "$SRC" "$D/acks.json"
t1=$(date +%s.%N)
events=$(python3 -I -c "import json,sys;print(sum(a['ack'].get('accepted',0) for a in json.load(open(sys.argv[1])) if isinstance(a['ack'],dict)))" "$D/acks.json")
echo "== $LABEL: $events events accepted in $(python3 -c "print(round($t1-$t0,1))") s (sequential post.py, one connection)"
kill $pid; wait $pid 2>/dev/null || true
du -b "$D/telemetry/e1"/{meta.sqlite3,core/*,detail/*} | grep -v -- '-wal\|-shm'
python3 -I "$ROOT/bench/storage/bytes.py" "$D/telemetry/e1"/core/*.sqlite3 "$D/telemetry/e1"/detail/*.sqlite3 --json "$D/bytes.json"
echo "data dir $D"
