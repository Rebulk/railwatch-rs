#!/usr/bin/env bash
# One memory run: a copy of a seeded data dir (seed.sh), the server on
# SERVER_CPUS, then phases, sampling the server every 2 s throughout:
#
#   idle0   IDLE s after /up
#   mixed   MIXED s of open-loop ingest (rw-load, RATE batches/s over every
#           environment) plus dashboard reads (dash.py, CLIENTS clients)
#   idle1   IDLE s with nothing (the idle-close timeouts get to act)
#   burst   BURST s of closed-loop ingest + 2x the clients, no pause
#   idle2   IDLE s
#
#   bench/memory/run.sh LABEL SEED_DIR [extra server env...]
#
# BIN_DIR picks the binaries. Writes OUT_ROOT/LABEL.XXXX/{samples.tsv,
# load-*.txt, dash-*.json, summary.txt}. A sample is: t, phase, RssAnon kB,
# RssFile kB, threads, open fds, open .sqlite3 fds, distinct .sqlite3 files.
set -euo pipefail
LABEL=${1:?usage: run.sh LABEL SEED_DIR [env...]} SEED=${2:?seed dir}; shift 2
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
BIN=${BIN_DIR:-$ROOT/target/release}
OUT_ROOT=${OUT_ROOT:-/tmp/rwmem/runs}
IDLE=${IDLE:-120} MIXED=${MIXED:-900} BURST=${BURST:-180} RATE=${RATE:-12} CLIENTS=${CLIENTS:-6} PAUSE=${PAUSE:-0.5}
PORT=${PORT:-18731}
mkdir -p "$OUT_ROOT"
R=$(mktemp -d "$OUT_ROOT/$LABEL.XXXX")
D=$R/data
cp -a --reflink=auto "$SEED" "$D"
export RAILWATCH_DATA_DIR=$D RAILWATCH_TRUST_PROXY=1 RAILWATCH_RATE_LIMIT_ENV=0 RAILWATCH_RATE_LIMIT_IP=0 PORT
export RAILWATCH_TRUSTED_PROXY_SECRET_FILE=$D/proxy.secret
env "$@" taskset -c "${SERVER_CPUS:-8-11}" "$BIN/railwatch" serve > "$R/server.log" 2>&1 &
pid=$!
trap 'kill $pid 2>/dev/null || true; kill $sampler 2>/dev/null || true' EXIT
until curl -sf "127.0.0.1:$PORT/up" > /dev/null; do kill -0 $pid 2> /dev/null || { echo "server exited (port $PORT in use?)" >&2; exit 1; }; sleep 0.2; done
echo idle0 > "$R/phase"
t0=$(date +%s)
(
  while kill -0 $pid 2>/dev/null; do
    st=/proc/$pid/status
    anon=$(awk '/^RssAnon/{print $2}' $st 2>/dev/null) file=$(awk '/^RssFile/{print $2}' $st 2>/dev/null) thr=$(awk '/^Threads/{print $2}' $st 2>/dev/null)
    links=$(find /proc/$pid/fd -mindepth 1 -maxdepth 1 -printf '%l\n' 2>/dev/null || true)
    fds=$(printf '%s\n' "$links" | grep -c . || true)
    sq=$(printf '%s\n' "$links" | grep -c '\.sqlite3$' || true)
    files=$(printf '%s\n' "$links" | grep '\.sqlite3$' | sort -u | grep -c . || true)
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' $(( $(date +%s) - t0 )) "$(cat "$R/phase")" "$anon" "$file" "$thr" "$fds" "$sq" "$files"
    sleep 2
  done
) > "$R/samples.tsv" &
sampler=$!
LOAD="taskset -c ${LOAD_CPUS:-12-15}"
URL=http://127.0.0.1:$PORT
sleep "$IDLE"
echo mixed > "$R/phase"
$LOAD "$BIN/rw-load" --url "$URL" --tokens "$D/tokens" --concurrency 8 --rate "$RATE" --duration "$MIXED" --xff-rotate 64 --seed 11 > "$R/load-mixed.txt" 2>&1 &
lp=$!
$LOAD python3 -I "$ROOT/bench/memory/dash.py" "$URL" "$D/proxy.secret" "$D/envs" "$CLIENTS" "$MIXED" "$PAUSE" > "$R/dash-mixed.json"
wait $lp || { echo "rw-load failed, see $R/load-*.txt" >&2; exit 1; }
echo idle1 > "$R/phase"
sleep "$IDLE"
echo burst > "$R/phase"
$LOAD "$BIN/rw-load" --url "$URL" --tokens "$D/tokens" --concurrency 12 --rate 0 --duration "$BURST" --xff-rotate 64 --seed 13 > "$R/load-burst.txt" 2>&1 &
lp=$!
$LOAD python3 -I "$ROOT/bench/memory/dash.py" "$URL" "$D/proxy.secret" "$D/envs" "$((CLIENTS * 2))" "$BURST" 0 > "$R/dash-burst.json"
wait $lp || { echo "rw-load failed, see $R/load-*.txt" >&2; exit 1; }
echo idle2 > "$R/phase"
sleep "$IDLE"
kill -TERM $pid; wait $pid || true
sleep 2
python3 -I "$ROOT/bench/memory/summary.py" "$R" | tee "$R/summary.txt"
[ -n "${KEEP_DATA:-}" ] || rm -rf "$D"
