#!/usr/bin/env bash
# Track C's max-50app case (bench/rails-vs-rust in the rails-vs-rust branch:
# 50 environments, rw-load closed loop, concurrency 64, 45 s, fresh data
# dir) without its sudo setup: the server runs in a user systemd scope with
# MemoryMax=3G and no swap, pinned to SERVER_CPUS (2 CPUs; Track C's slice
# holds 6-7 exclusively). Data on disk (DATA_ROOT), not tmpfs: tmpfs pages
# count against the scope and cannot be reclaimed without swap, so a fast
# run was OOM-killed at 3 GiB. RAILWATCH_SQLITE_SYNC=off (the bench knob,
# schema::configure) keeps a shared disk's fsync stalls out of it; Track C
# used a 1 ms-delay zram device instead. Prints one JSON line: throughput,
# latency, peak and after-load cgroup anon (memory.stat "anon", sampled
# every 0.5 s), CPU seconds.
#
#   bench/memory/apps50.sh LABEL [extra server env...]
set -euo pipefail
LABEL=${1:?usage: apps50.sh LABEL [env...]}; shift
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
BIN=${BIN_DIR:-$ROOT/target/release}
APPS=${APPS:-50} CONC=${CONC:-64} DUR=${DUR:-45} PORT=${PORT:-18751}
# the previous run's scope may still be stopping
for _ in $(seq 1 120); do ss -ltn | grep -q "127.0.0.1:$PORT " || break; sleep 0.5; done
mkdir -p "${DATA_ROOT:-$HOME/.cache/rwmem}"
D=$(mktemp -d "${DATA_ROOT:-$HOME/.cache/rwmem}/rw-apps50.XXXX")
unit=rw-apps50-$$
mkdir -p /tmp/rwmem
cg=
trap '{ [ -z "$cg" ] || cat "$cg/memory.events" > /tmp/rwmem/last-events; } 2>/dev/null || true; cp "$D/server.log" /tmp/rwmem/last-server.log 2>/dev/null || true; systemctl --user stop "$unit.scope" 2>/dev/null || true; rm -rf "$D"' EXIT
for i in $(seq 1 "$APPS"); do RAILWATCH_DATA_DIR=$D "$BIN/railwatch" env create "e$i" | grep -o 'rw_[A-Za-z0-9]*'; done > "$D/tokens"
systemd-run --user --scope --unit "$unit" -p MemoryMax=3G -p MemorySwapMax=0 -p AllowedCPUs="${SERVER_CPUS:-0-1}" \
  env RAILWATCH_DATA_DIR="$D" PORT="$PORT" RAILWATCH_TRUST_PROXY=1 RAILWATCH_RATE_LIMIT_ENV=0 RAILWATCH_SQLITE_SYNC=off "$@" \
  taskset -c "${SERVER_CPUS:-0-1}" "$BIN/railwatch" serve > "$D/server.log" 2>&1 &
spid=$!
until curl -sf "127.0.0.1:$PORT/up" > /dev/null; do kill -0 $spid 2> /dev/null || { cat "$D/server.log" >&2; exit 1; }; sleep 0.2; done
cg=/sys/fs/cgroup/$(systemctl --user show -p ControlGroup --value "$unit.scope")
anon() { awk '/^anon /{print $2}' "$cg/memory.stat"; }
cpu() { awk '/^usage_usec/{print $2}' "$cg/cpu.stat"; }
sleep 10
idle=$(anon)
( while :; do anon; sleep 0.5; done ) > "$D/mem" &
sampler=$!
c0=$(cpu)
taskset -c "${LOAD_CPUS:-2-5}" "$BIN/rw-load" --url "http://127.0.0.1:$PORT" --tokens "$D/tokens" --concurrency "$CONC" --rate 0 \
  --duration "$DUR" --xff-rotate 256 --seed 1 > "$D/load.txt" 2>&1
# tail: until the scope is idle 6 s in a row (max 120 s), as Track C does
quiet=0 t=0 prev=$(cpu)
while [ $quiet -lt 6 ] && [ $t -lt 120 ]; do
  sleep 1; t=$((t + 1)); now=$(cpu)
  if [ $((now - prev)) -lt 50000 ]; then quiet=$((quiet + 1)); else quiet=0; fi
  prev=$now
done
c1=$(cpu)
kill $sampler
kill -0 $spid 2> /dev/null || { echo "server died: $(cat "$cg/memory.events" 2>/dev/null | tr '\n' ' ')" >&2; exit 1; }
after=$(anon)
peak=$(sort -n "$D/mem" | tail -1)
python3 -I - "$LABEL" "$D/load.txt" "$idle" "$peak" "$after" "$(( (c1 - c0) / 1000000 ))" <<'PY'
import json, re, sys
label, load, idle, peak, after, cpu = sys.argv[1:]
s = open(load).read()
m = re.search(r"events/s accepted: (\d+)", s)
st = re.search(r"^\s+200\s+(\d+)\s+([\d.]+)\s+([\d.]+)\s+([\d.]+)", s, re.M)
other = sum(int(x) for x in re.findall(r"^\s+(?!200)(?:\d{3}|error)\s+(\d+)", s, re.M))
mib = lambda b: round(int(b) / 2**20)
print(json.dumps({"label": label, "events_s": int(m.group(1)) if m else None,
  "p50_ms": float(st.group(2)) if st else None, "p99_ms": float(st.group(4)) if st else None, "non200": other,
  "anon_idle_mib": mib(idle), "anon_peak_mib": mib(peak), "anon_after_mib": mib(after), "cpu_s": int(cpu)}))
PY
