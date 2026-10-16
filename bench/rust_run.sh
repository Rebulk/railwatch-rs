#!/usr/bin/env bash
# One closed-loop rw-load run against a fresh Rust server, under the same
# conditions as bench/rails_run.sh: data on the same disk as the Rails docker
# volume (not tmpfs), server pinned to SERVER_CPUS, load to LOAD_CPUS, the
# per-IP limiter on (2,400/min, XFF rotated across 256 addresses so it runs
# but never trips). The per-environment limit (600/min) is off by default
# because rw-load's closed loop would hit it; Rails' is on but every run
# stays below it per env -- check the 429 count in the output.
#   RATE=<batches/s> for open loop (default 0 = closed loop)
#   bench/rust_run.sh <label> <envs> <concurrency> <duration> [extra server env...]
# Prints rw-load's summary, peak private memory (RssAnon; mmap'd tenant file
# pages are page cache, excluded on both sides) and tenant file sizes.
set -euo pipefail
cd "$(dirname "$0")/.."
label=$1 envs=$2 conc=$3 dur=$4; shift 4
root=${BENCH_DATA_ROOT:-$PWD/bench/scratch}   # /dev/shm/rwrs for the RAM-disk runs
mkdir -p "$root"
D=$(mktemp -d "$root/rust-$label.XXXX")
export RAILWATCH_DATA_DIR=$D RAILWATCH_TRUST_PROXY=1 RAILWATCH_RATE_LIMIT_ENV=${RAILWATCH_RATE_LIMIT_ENV:-0} PORT=${PORT:-8795}
for i in $(seq 1 "$envs"); do ./target/release/railwatch env create "e$i" | grep -o 'rw_[A-Za-z0-9]*'; done > "$D/tokens"
env "$@" taskset -c "${SERVER_CPUS:-8-11}" ./target/release/railwatch serve > "$D/server.log" 2>&1 &
pid=$!
trap 'kill $pid 2>/dev/null || true' EXIT
until curl -sf "127.0.0.1:$PORT/up" >/dev/null; do sleep 0.1; done
idle=$(awk '/RssAnon/{print $2}' /proc/$pid/status)
( while kill -0 $pid 2>/dev/null; do awk '/RssAnon/{print $2}' /proc/$pid/status 2>/dev/null || true; sleep 0.5; done ) > "$D/rss" &
t0=$(awk '{print $14+$15}' /proc/$pid/stat)
taskset -c "${LOAD_CPUS:-12-15}" ./target/release/rw-load --url "http://127.0.0.1:$PORT" --tokens "$D/tokens" --concurrency "$conc" --rate "${RATE:-0}" \
  --duration "$dur" --xff-rotate 256 --out "$D/result.json" 2>&1 | grep -E 'batches/s|^ +[0-9]{3} ' || true
t1=$(awk '{print $14+$15}' /proc/$pid/stat)
peak=$(sort -n "$D/rss" | tail -1)
echo "== $label: anon idle $((idle / 1024)) MiB peak $((peak / 1024)) MiB; CPU s $(( (t1 - t0) / $(getconf CLK_TCK) )); tenant bytes $(du -cb "$D"/telemetry/*.sqlite3* | tail -1 | cut -f1); dir $D"
