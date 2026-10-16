#!/usr/bin/env bash
# Profiles the server under rw-load ingest and prints CPU-us per event.
#   bench/flame/ingest.sh <label> <envs> <conc> <dur> [rate]   (rate 0 = closed loop)
# Env: BIN_DIR (default target-prof/release, built with frame pointers),
# PORT (8801), PERF=fp|dwarf|none (fp), SERVER_CPUS 8-11, LOAD_CPUS 12-15,
# OUT (bench/results/flame), PERF_DELAY (s into the run before sampling starts).
# Every second the WAL size is appended to $D/wal (KEEP=1 to look at it). Data on /dev/shm (RAM; measures the server).
# Kills only the server it started (by pid). The data dir is removed on
# exit (/dev/shm has a per-user quota) unless KEEP=1.
set -euo pipefail
export PATH="$PATH:$HOME/.cargo/bin"
cd "$(dirname "$0")/../.."
label=$1 envs=$2 conc=$3 dur=$4 rate=${5:-0}
BIN=${BIN_DIR:-target-prof/release} PORT=${PORT:-8801} OUT=${OUT:-bench/results/flame}
mkdir -p "$OUT" ${SHM:-/dev/shm/rwperf}
D=$(mktemp -d ${SHM:-/dev/shm/rwperf}/$label.XXXX)
export RAILWATCH_DATA_DIR=$D RAILWATCH_TRUST_PROXY=1 RAILWATCH_RATE_LIMIT_ENV=0 PORT
for i in $(seq 1 "$envs"); do "$BIN/railwatch" env create "e$i" | grep -o 'rw_[A-Za-z0-9]*'; done > "$D/tokens"
# LAYOUT=single: give each environment an empty single file instead of the
# per-day directory new environments get.
if [ "${LAYOUT:-}" = single ]; then
  for i in $(seq 1 "$envs"); do
    "$BIN/railwatch" export "e$i" "$D/telemetry/e$i.sqlite3" >/dev/null && rm -rf "$D/telemetry/e$i"
  done
fi
env ${SERVER_ENV:-} taskset -c "${SERVER_CPUS:-8-11}" "$BIN/railwatch" serve > "$D/server.log" 2>&1 &
pid=$!
trap 'kill $pid 2>/dev/null || true; wait $pid 2>/dev/null || true; [ -n "${KEEP:-}" ] || rm -rf "$D"' EXIT
until curl -sf "127.0.0.1:$PORT/up" >/dev/null; do sleep 0.1; done
# warm-up: open writers, first pages
taskset -c "${LOAD_CPUS:-12-15}" "$BIN/rw-load" --url "http://127.0.0.1:$PORT" --tokens "$D/tokens" --concurrency "$conc" --rate "$rate" --duration 3 --xff-rotate 256 --out "$D/warm.json" >/dev/null 2>&1
t0=$(awk '{print $14+$15}' /proc/$pid/stat); cs0=$(awk '/ctxt/{s+=$2} END{print s}' /proc/$pid/task/*/status 2>/dev/null || echo 0)
case ${PERF:-fp} in
  fp)    ( sleep "${PERF_DELAY:-0}"; perf record -F 999 -g --call-graph fp -p $pid -o "$D/perf.data" -- sleep "$((dur - ${PERF_DELAY:-0}))" >/dev/null 2>&1 ) & ;;
  dwarf) ( sleep "${PERF_DELAY:-0}"; perf record -F 299 --call-graph dwarf,16384 -p $pid -o "$D/perf.data" -- sleep "$((dur - ${PERF_DELAY:-0}))" >/dev/null 2>&1 ) & ;;
  none)  sleep 0 & ;;
esac
pp=$!
# Per second: time, WAL bytes, server major faults (swap-ins of /dev/shm pages
# show up here), rchar, wchar.
( while kill -0 $pid 2>/dev/null; do echo "$(date +%s) $(find "$D"/telemetry -name "*-wal" -printf "%s\n" 2>/dev/null | awk "{s+=\$1} END{print s+0}") $(awk '{print $12}' /proc/$pid/stat) $(awk '/^rchar|^wchar/{printf "%s ", $2}' /proc/$pid/io)"; sleep 1; done ) > "$D/wal" &
taskset -c "${LOAD_CPUS:-12-15}" "$BIN/rw-load" --url "http://127.0.0.1:$PORT" --tokens "$D/tokens" --concurrency "$conc" --rate "$rate" \
  --duration "$dur" --xff-rotate 256 --out "$D/result.json" 2>&1 | grep -E 'batches/s|p50|p99|^ +[0-9]{3} ' || true
t1=$(awk '{print $14+$15}' /proc/$pid/stat)
# CPU seconds by thread name over the whole run (writers summed).
threads=$(for t in /proc/$pid/task/*; do echo "$(sed 's/-e[0-9]*$//;s/[0-9]*$//' $t/comm) $(awk '{print $14+$15}' $t/stat)"; done |
  awk -v hz="$(getconf CLK_TCK)" '{c[$1]+=$2} END{for(k in c) if(c[k]>0) printf "%s %.1fs ", k, c[k]/hz}')
wait $pp || true
ev=$(python3 -c "import json,sys;r=json.load(open(sys.argv[1]));print(int(r['batches_ok']*r['records_per_batch']))" "$D/result.json")
  lat=$(python3 -c "import json,sys;r=json.load(open(sys.argv[1]));l=r['latency'].get('200',{});print(f\"ev/s {r['events_per_s']:.0f} p50 {l.get('p50_ms',0):.1f} p99 {l.get('p99_ms',0):.1f} max {l.get('max_ms',0):.1f} ms\")" "$D/result.json")
cpu_ms=$(( (t1 - t0) * 1000 / $(getconf CLK_TCK) ))
echo "== $label: $lat"
echo "== $label: thread CPU (whole run) $threads"
echo "== $label: server CPU ${cpu_ms} ms, events $ev, CPU-us/event $(python3 -c "print(round($cpu_ms*1000/max($ev,1),2))"), rss $(awk '/VmRSS/{print $2}' /proc/$pid/status) kB, dir $D"
grep -E 'ERROR|WARN' "$D/server.log" | head -5 || true
if [ "${PERF:-fp}" != none ]; then
  mkdir -p target-prof/flame
  perf script -i "$D/perf.data" 2>/dev/null | inferno-collapse-perf > "target-prof/flame/$label.folded"
  inferno-flamegraph --title "$label" < "target-prof/flame/$label.folded" > "$OUT/$label.svg"
  echo "svg $OUT/$label.svg  folded target-prof/flame/$label.folded"
fi
