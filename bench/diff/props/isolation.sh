#!/usr/bin/env bash
# Does a slow dashboard page starve ingest? Rust: ingest has one writer
# thread per tenant; pages read on their own read-only connections (WAL), so
# they should not. Measured:
#
#   bench/diff/props/isolation.sh TENANT.sqlite3 [page=queries_index] [window=7d] [threads=8] [seconds=40] [rate=40]
#
# 1. A Rust server on a COPY of TENANT (the original is not written to).
# 2. rw-load at a fixed open-loop rate (batches/s) for `seconds`: baseline.
# 3. The same load again while page-hammer runs `page` on `threads` threads
#    against the same file for the whole run.
# Prints ingest p50/p99 for both runs and the pages completed meanwhile.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../.." && pwd)
SRC=${1:?usage: isolation.sh TENANT [page] [window] [threads] [seconds] [rate]}
PAGE=${2:-queries_index} WINDOW=${3:-7d} THREADS=${4:-8} SECS=${5:-40} RATE=${6:-40}
PORT=${PORT:-8797}
cargo build -q --release -p railwatch-server -p railwatch-load --manifest-path "$ROOT/Cargo.toml"
BIN=$ROOT/target/release
# The tenant copy is GBs: ISO_DIR picks where (default target/; /tmp if it fits).
WORK=$(mktemp -d "${ISO_DIR:-$ROOT/target}/rw-iso.XXXX")
export RAILWATCH_DATA_DIR=$WORK/data
TOKEN=$("$BIN/railwatch" env create iso --retention-days 30)
cp "$SRC" "$WORK/data/telemetry/iso.sqlite3"
rm -f "$WORK/data/telemetry/iso.sqlite3-wal" "$WORK/data/telemetry/iso.sqlite3-shm"
PORT=$PORT RAILWATCH_RATE_LIMIT_IP=0 RAILWATCH_RATE_LIMIT_ENV=0 "$BIN/railwatch" serve > "$WORK/serve.log" 2>&1 &
SERVER=$!
trap 'kill $SERVER 2>/dev/null || true; rm -rf "$WORK"' EXIT
for _ in $(seq 1 50); do curl -fsS -o /dev/null "http://127.0.0.1:$PORT/up" && break; sleep 0.1; done

load() {
  "$BIN/rw-load" --url "http://127.0.0.1:$PORT" --token "$TOKEN" --concurrency 16 --rate "$RATE" --duration "$SECS" --seed "$1" --out "$WORK/load-$1.json" 2>&1 \
    | grep -E "batches/s|^ +200 " | head -3
}
echo "== ingest alone ($RATE batches/s x 500 records, ${SECS}s)"
load 21
echo "== ingest while $THREADS threads hammer $PAGE?window=$WINDOW"
"$BIN/page-hammer" "$WORK/data/telemetry/iso.sqlite3" "$PAGE" "$WINDOW" "$THREADS" "$((SECS + 2))" > "$WORK/hammer.txt" &
HAMMER=$!
sleep 1
load 22
wait $HAMMER
echo "== pages completed during the loaded run: $(cat "$WORK/hammer.txt")"
python3 -I - "$WORK/load-21.json" "$WORK/load-22.json" <<'PY'
import json, sys
for label, p in zip(["alone", "hammered"], sys.argv[1:]):
    r = json.load(open(p))
    keys = {k: r[k] for k in r if any(s in k for s in ("p50", "p99", "max", "status", "events"))}
    print(label, json.dumps(keys))
PY
