#!/usr/bin/env bash
# One step of the app-count sweep: a FRESH server (new data dir, N new apps)
# in bench-server.slice, then rw-load --profile for DURATION seconds of
# production-shaped, open-loop traffic. Prints one JSON line and appends it
# to $RESULTS. Needs reserve.sh on and disk.sh up first (sweep.sh does both).
#
#   bench/scale/step.sh rails|rust|rust-memory APPS [BUSY_EVERY]
#
# Sampled every SAMPLE_S (2) seconds while the load runs: slice CPU, anon,
# memory.current, summed VmRSS and open fds of the server processes, Rails'
# Solid Queue backlog (ready jobs, oldest ready age), and how busy the 24
# unreserved CPUs are (/proc/stat), for the noise notes.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
export RVR_SLICE=bench-server.slice
export RVR_SLICE_PATH=/sys/fs/cgroup/bench.slice/bench-server.slice
export SERVER_CPUS=${SERVER_CPUS:-14,30}
export BENCH_MNT=${BENCH_MNT:-/var/tmp/rw-scale/mnt}
BIN=${BIN:-$HOME/.cache/rw-scale/bin}
export LOAD_CPUS=${LOAD_CPUS:-12,13,15,28,29,31}
# docker needs root on the bench box (not in the docker group)
[ -w /var/run/docker.sock ] || docker() { sudo docker "$@"; }
. "$HERE/../rails-vs-rust/lib.sh"
LOAD_BIN=$BIN/rw-load
side=$1 apps=$2 busy_every=${3:-0}
case "$side" in
rust) RUST_BIN=$BIN/railwatch-main ;;
rust-memory) RUST_BIN=$BIN/railwatch-memory ;;
rails) ;;
*) echo "side: rails|rust|rust-memory" >&2; exit 2 ;;
esac
DURATION=${DURATION:-600} SAMPLE_S=${SAMPLE_S:-2}
RESULTS=${RESULTS:-$RVR_ROOT/bench/results/scale/steps.jsonl}
mkdir -p "$(dirname "$RESULTS")"
run=$(date +%Y%m%dT%H%M%S)-$side-$apps-b$busy_every
data=$MNT/$run
work=$(mktemp -d "/tmp/rw-scale-$run.XXXX")
rvr_stop
sudo find "$MNT" -mindepth 1 -maxdepth 1 -name '20*' -exec rm -rf {} +

echo "== $run: starting server, seeding $apps apps ($(date +%T))" >&2
t_seed=$(date +%s)
if [ "$side" = rails ]; then
  rvr_start_rails "$data"
  rvr_seed_rails "$apps" "$work/tokens"
  url=http://127.0.0.1:$RAILS_PORT
else
  rvr_start_rust "$data" "$apps" "$work/tokens"
  url=http://127.0.0.1:$RUST_PORT
fi
seed_s=$(( $(date +%s) - t_seed ))
sleep "${IDLE_SETTLE:-20}"
idle_anon=$(rvr_anon)
disk0=$(du -sb "$data" | cut -f1)

pids() { find "$SLICE" -name cgroup.procs -exec cat {} + 2>/dev/null; }
rss_kb() { local t=0; for p in $(pids); do r=$(awk '/^VmRSS/{print $2}' "/proc/$p/status" 2>/dev/null || echo 0); t=$((t + ${r:-0})); done; echo $t; }
fds() { local t=0; for p in $(pids); do n=$(sudo ls "/proc/$p/fd" 2>/dev/null | wc -l); t=$((t + n)); done; echo $t; }
# busy jiffies / total jiffies of the unreserved CPUs since the last call
host_busy() { awk '/^cpu[0-9]/{c=substr($1,4)+0; if (c<12 || (c>=16 && c<28)) {t=0; for(i=2;i<=NF;i++) t+=$i; b+=t-$5-$6; a+=t}} END{print b, a}' /proc/stat; }
queue() {
  [ "$side" = rails ] || { echo "- -"; return; }
  sqlite3 "file:$data/production_queue.sqlite3?mode=ro" \
    "SELECT count(*), coalesce(CAST((julianday('now') - julianday(min(created_at))) * 86400 AS INT), 0) FROM solid_queue_ready_executions" 2>/dev/null | tr '|' ' ' || echo "? ?"
}

read -r hb0 ha0 < <(host_busy)
cpu0=$(rvr_cpu_us)
date +%s.%N > "$work/load_t0"
( while :; do
    read -r q qage < <(queue)
    echo "$(date +%s) $(rvr_cpu_us) $(rvr_anon) $(rvr_mem_current) $(rss_kb) $(fds) $q $qage $(cut -d' ' -f1 /proc/loadavg)"
    sleep "$SAMPLE_S"
  done ) > "$work/samples" &
sampler=$!
python3 -I "$HERE/fsync_probe.py" "$MNT" > "$work/fsync" &
probe=$!
# disk at the middle of the load window: growth over the second half is the
# steady rate (the first minutes include each new file's fixed size)
# db_bytes: the same without -wal/-shm files (a WAL is reused, not grown, once
# it reaches its checkpoint size, so on fresh files it inflates a short run)
db_bytes() { find "$data" -type f ! -name "*-wal" ! -name "*-shm" -printf "%s\n" | awk "{t+=\$1} END{print t+0}"; }
( sleep $((DURATION / 2)); du -sb "$data" | cut -f1 > "$work/disk_mid"; db_bytes > "$work/db_mid" ) &
trap 'kill $sampler $probe 2>/dev/null || true' EXIT
echo "== $run: load ${DURATION}s ($(date +%T))" >&2
# in bench-load.slice: the caller sits in user.slice, which reserve.sh keeps
# off the reserved CPUs, so a plain taskset there is refused.
sudo systemd-run --scope --quiet --slice=bench-load.slice --uid="$(id -u)" --gid="$(id -g)" -- "$LOAD_BIN" --profile "$HERE/profiles.json" --url "$url" --tokens "$work/tokens" \
  --duration "$DURATION" --warmup "${WARMUP:-60}" --busy-every "$busy_every" --seed "${SEED:-1}" --out "$work/load.json" > "$work/load.txt" 2>&1
cpu1=$(rvr_cpu_us)
kill $probe 2>/dev/null || true
read -r hb1 ha1 < <(host_busy)
# tail: deferred work after the load stops (Rails' jobs, Rust's rollup flush).
# Idle = under 5% of one core for 6 s in a row; at most TAIL_MAX s.
quiet=0 t=0 prev=$(rvr_cpu_us)
while [ $quiet -lt 6 ] && [ $t -lt "${TAIL_MAX:-300}" ]; do
  sleep 1; t=$((t+1)); now=$(rvr_cpu_us)
  if [ $((now - prev)) -lt 50000 ]; then quiet=$((quiet+1)); else quiet=0; fi
  prev=$now
done
tail_idle=$([ $quiet -ge 6 ] && echo True || echo False)
kill $sampler 2>/dev/null || true
cpu2=$(rvr_cpu_us)
read -r q_end qage_end < <(queue)
after_anon=$(rvr_anon)
disk1=$(du -sb "$data" | cut -f1)
tel1=$(du -sb "$data/telemetry" | cut -f1)
db1=$(db_bytes)
oom=$(awk '/^oom_kill /{print $2}' "$SLICE/memory.events")
failed_jobs=None
[ "$side" = rails ] && failed_jobs=$(sqlite3 "file:$data/production_queue.sqlite3?mode=ro" "SELECT count(*) FROM solid_queue_failed_executions")
exited=$(docker ps -a --filter "name=rvr-" --filter status=exited --format '{{.Names}}' | tr '\n' ' ')

python3 -I - "$work/load.json" "$work/samples" "$work/fsync" <<PY | tee -a "$RESULTS"
import json, sys
r = json.load(open(sys.argv[1]))
S = [l.split() for l in open(sys.argv[2]) if len(l.split()) == 9]
num = lambda v: None if v in ("-", "?") else int(v)
anon = [int(s[2]) for s in S]; cur = [int(s[3]) for s in S]; rss = [int(s[4]) for s in S]; fd = [int(s[5]) for s in S]
qn = [num(s[6]) for s in S]; qa = [num(s[7]) for s in S]
# steady = median of the last half of the load window
half = lambda v: sorted(v[len(v)//2:])[len(v[len(v)//2:])//2] if v else None
# CPU % of the 2 server threads, per sample interval, steady half
cpu = [(int(b[1]) - int(a[1])) / 1e6 / max(1, int(b[0]) - int(a[0])) / 2 * 100 for a, b in zip(S, S[1:])]
lat = r["latency"]
dur = $DURATION; apps = $apps
lag_q = max([a for a in qa if a is not None], default=None)
row = {
  "run": "$run", "disk_mode": "${DISK_MODE:-loop}", "side": "$side", "apps": apps, "busy_every": $busy_every, "busy_apps": r["busy_apps"], "duration_s": dur,
  "seed_s": $seed_s, "posts": r["posts"], "batches_per_s": round(r["batches_per_s"], 1), "records_per_s": round(r["records_per_s"]),
  "status_counts": r["status_counts"], "not_ok": r["not_ok"], "apps_with_errors": r["apps_with_errors"],
  "p50_ms": round(lat["p50_ms"], 1), "p99_ms": round(lat["p99_ms"], 1), "p999_ms": round(lat["p999_ms"], 1), "max_ms": round(lat["max_ms"], 1),
  "data_p50_ms": round(r["latency_data"]["p50_ms"], 1), "data_p99_ms": round(r["latency_data"]["p99_ms"], 1),
  "health_p99_ms": round(r["latency_health"]["p99_ms"], 1), "over_3s": r["over_3s"], "max_in_flight": r["max_in_flight"],
  "worst_10s_p99_ms": round(max(w["p99_ms"] for w in r["timeline_10s"]), 1),
  "cpu_pct_avg": round(($cpu1 - $cpu0) / 1e6 / dur / 2 * 100, 1), "cpu_pct_steady": round(half(cpu), 1) if cpu else None,
  "cpu_pct_max_2s": round(max(cpu), 1) if cpu else None,
  "cpu_tail_s": round(($cpu2 - $cpu1) / 1e6, 1), "tail_s": $t, "tail_idle": $tail_idle,
  "jobs_ready_max": max([q for q in qn if q is not None], default=None), "jobs_oldest_ready_max_s": lag_q,
  "jobs_ready_end": num("$q_end"), "failed_jobs": $failed_jobs,
  "idle_anon_mib": round($idle_anon / 2**20), "peak_anon_mib": round(max(anon) / 2**20), "steady_anon_mib": round(half(anon) / 2**20),
  "after_anon_mib": round($after_anon / 2**20), "peak_current_mib": round(max(cur) / 2**20),
  "peak_rss_mib": round(max(rss) / 1024), "steady_rss_mib": round(half(rss) / 1024),
  "peak_fds": max(fd), "steady_fds": half(fd), "oom_kills": int("${oom:-0}"), "exited_containers": "$exited".strip(),
  "disk_seeded_bytes": $disk0, "disk_bytes": $disk1, "telemetry_bytes": $tel1,
  "disk_mid_bytes": $(cat "$work/disk_mid" 2>/dev/null || echo 0),
  "records_ok": r["records_ok"],
  "db_mid_bytes": $(cat "$work/db_mid" 2>/dev/null || echo 0), "db_bytes": $db1,
  "db_per_app_day_mib": round(($db1 - $(cat "$work/db_mid" 2>/dev/null || echo 0)) / apps * 86400 / (dur / 2) / 2**20, 1),
  # bytes per stored record over the whole run (fresh files, so an upper bound)
  "bytes_per_record": round(($disk1 - $disk0) / max(1, r["records_ok"]), 1),
  # steady growth over the second half of the load, per app, extrapolated to a day
  "disk_per_app_day_mib": round(($disk1 - $(cat "$work/disk_mid" 2>/dev/null || echo 0)) / apps * 86400 / (dur / 2) / 2**20, 1),
  "host_busy_pct_unreserved": round(($hb1 - $hb0) / max(1, ($ha1 - $ha0)) * 100, 1),
  "loadavg_max": max(float(s[8]) for s in S),
}
# breaks (defined in docs/SCALE.md): p99 > 1 s, any drop, > 60 s behind, OOM
why = []
if row["p99_ms"] > 1000: why.append("p99>1s")
if row["not_ok"] > 0: why.append("drops")
if (lag_q or 0) > 60 or not row["tail_idle"] or row["tail_s"] > 60: why.append("behind>60s")
if row["oom_kills"] > 0 or row["exited_containers"]: why.append("oom/exit")
P = [l.split() for l in open(sys.argv[3]) if len(l.split()) == 2]
F = sorted(float(x[1]) for x in P)
fq = lambda p: round(F[min(len(F) - 1, int(p * len(F)))], 1) if F else None
row.update({"fsync_probe_n": len(F), "fsync_p50_ms": fq(0.5), "fsync_p99_ms": fq(0.99), "fsync_max_ms": fq(1.0) if F else None,
            "fsync_over_1s": sum(1 for x in F if x > 1000)})
row["breaks"] = why
print(json.dumps(row))
PY
mkdir -p "$(dirname "$RESULTS")/raw"
cp "$work/load.json" "$(dirname "$RESULTS")/raw/$run.load.json"
cp "$work/samples" "$(dirname "$RESULTS")/raw/$run.samples"
cp "$work/fsync" "$(dirname "$RESULTS")/raw/$run.fsync" 2>/dev/null || true
cp "$work/load_t0" "$(dirname "$RESULTS")/raw/$run.t0" 2>/dev/null || true
[ -n "${KEEP_SERVER:-}" ] || rvr_stop
rm -rf "$work"
