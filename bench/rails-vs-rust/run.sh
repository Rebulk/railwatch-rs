#!/usr/bin/env bash
# One ingest run against a FRESH server (new data dir, new tenants), inside
# rvr.slice (host.sh up first). Prints one JSON line to stdout and appends
# it to $RESULTS (default bench/results/rails-vs-rust/runs.jsonl).
#
#   bench/rails-vs-rust/run.sh rails|rust LABEL APPS CONCURRENCY DURATION [RATE]
#
# RATE = batches/s open loop (each batch 500 records); 0 or absent = closed
# loop (max throughput). Measured:
# - load average and host IO pressure (/proc/pressure/io "full" avg10)
#   right before the run;
# - idle memory: slice anon bytes after start + seeding + IDLE_SETTLE s;
# - during the load window: rw-load's throughput and latency, slice CPU,
#   peak slice anon and peak memory.current (anon + page cache) sampled
#   every 0.5 s, fsync/fdatasync syscalls made by the slice (perf stat -G);
# - after the load stops: the slice keeps running until its CPU use drops
#   below 5% of one core for 6 s in a row (max TAIL_MAX s), so deferred work
#   (Rails' Solid Queue rollups and grouping, Rust's 5 s rollup flush) is
#   counted. cpu_total_s includes it;
# - after the tail: anon (memory after load), bytes of the telemetry files
#   on disk, bytes written to the device (cgroup io.stat).
set -euo pipefail
. "$(dirname "$0")/lib.sh"
side=$1 label=$2 apps=$3 conc=$4 dur=$5 rate=${6:-0}
RESULTS=${RESULTS:-$RVR_ROOT/bench/results/rails-vs-rust/runs.jsonl}
mkdir -p "$(dirname "$RESULTS")"
run=$(date +%Y%m%dT%H%M%S)-$side-$label
data=$MNT/$run
work=$(mktemp -d "/tmp/rvr-$run.XXXX")
rvr_stop
# keep the bench disk from filling: earlier runs' data is not needed once
# their line is in $RESULTS (KEEP_DATA=1 keeps it)
[ -n "${KEEP_DATA:-}" ] || sudo find "$MNT" -mindepth 1 -maxdepth 1 -name '20*' -exec rm -rf {} +

if [ "$side" = rails ]; then
  rvr_start_rails "$data"
  rvr_seed_rails "$apps" "$work/tokens"
  url=http://127.0.0.1:$RAILS_PORT
  tel=$data/telemetry
else
  rvr_start_rust "$data" "$apps" "$work/tokens"
  url=http://127.0.0.1:$RUST_PORT
  tel=$data/telemetry
fi
sleep "${IDLE_SETTLE:-20}"
idle_anon=$(rvr_anon) idle_cur=$(rvr_mem_current)
load_before=$(rvr_load) iopsi_before=$(rvr_iopsi)

( while :; do echo "$(rvr_anon) $(rvr_mem_current)"; sleep 0.5; done ) > "$work/mem" &
sampler=$!
sudo perf stat -a -x, -e syscalls:sys_enter_fsync,syscalls:sys_enter_fdatasync -G rvr.slice,rvr.slice -o "$work/perf" &
perfpid=$!
trap 'kill $sampler 2>/dev/null || true; sudo kill -INT $perfpid 2>/dev/null || true' EXIT
read -r io_b0 io_n0 < <(rvr_io)
cpu0=$(rvr_cpu_us)
taskset -c "$LOAD_CPUS" "$LOAD_BIN" --url "$url" --tokens "$work/tokens" --concurrency "$conc" --rate "$rate" \
  --duration "$dur" --xff-rotate 256 --seed "${SEED:-1}" --out "$work/result.json" > "$work/load.txt" 2>&1
cpu1=$(rvr_cpu_us)
kill $sampler
sudo kill -INT $perfpid; wait $perfpid 2>/dev/null || true
# Rails debounces RollupJob to one enqueue per (environment, hour) per 60 s
# and leaves the rest of the hour to RollupCatchupJob (every 5 minutes), so
# right after a 45 s run its rollups do not yet cover the run. Enqueue the
# catchup now -- from a throwaway container OUTSIDE the slice, so its Rails
# boot is not counted -- and let the jobs container inside the slice do the
# work, as production would within 5 minutes. (Rust folds every batch into
# its rollups incrementally and flushes every 5 s: no equivalent step.)
if [ "$side" = rails ]; then
  docker run --rm --network host --user 1000:1000 --cpuset-cpus "$LOAD_CPUS" --env-file "$SECRETS" \
    -e RAILWATCH_ALLOWED_HOSTS=127.0.0.1,localhost -v "$data:/rails/storage" "$RAILS_IMAGE" \
    bin/rails runner 'RollupCatchupJob.perform_now' >/dev/null 2>&1 || { echo "rollup catchup enqueue failed: no result (its CPU would be missing)" >&2; exit 1; }
fi
# tail: until the slice is idle (deferred work done). 6 quiet seconds, so
# Rust's 5 s rollup flush cannot fall in a gap.
quiet=0 t=0 prev=$(rvr_cpu_us)
while [ $quiet -lt 6 ] && [ $t -lt "${TAIL_MAX:-120}" ]; do
  sleep 1; t=$((t+1)); now=$(rvr_cpu_us)
  if [ $((now - prev)) -lt 50000 ]; then quiet=$((quiet+1)); else quiet=0; fi
  prev=$now
done
cpu2=$(rvr_cpu_us)
read -r io_b1 io_n1 < <(rvr_io)
after_anon=$(rvr_anon)
disk=$(du -sb "$tel" | cut -f1)
peak_anon=$(awk '{print $1}' "$work/mem" | sort -n | tail -1)
peak_cur=$(awk '{print $2}' "$work/mem" | sort -n | tail -1)
fsync=$(awk -F, '/sys_enter_fsync/{print $1}' "$work/perf"); fdsync=$(awk -F, '/sys_enter_fdatasync/{print $1}' "$work/perf")
# Rails: jobs that failed (their work would be missing from the CPU count)
# and jobs still queued after the tail
failed_jobs=None pending_jobs=None gc_groups=None gc_batches=None
if [ "$side" = rails ]; then
  # counterfactual.sh only: transactions and batches the group commit made
  # (cumulative per Puma worker, logged every 20 groups; last line per pid)
  gc=$(docker logs rvr-rails 2>&1 | sed -n 's/.*BENCH_GROUP_COMMIT pid=\([0-9]*\) groups=\([0-9]*\) batches=\([0-9]*\).*/\1 \2 \3/p' \
    | awk '{g[$1]=$2; b[$1]=$3} END {for (p in g) {G+=g[p]; B+=b[p]} if (G) print G, B}')
  if [ -n "$gc" ]; then read -r gc_groups gc_batches <<< "$gc"; fi
  failed_jobs=$(sqlite3 "file:$data/production_queue.sqlite3?mode=ro" "SELECT count(*) FROM solid_queue_failed_executions")
  pending_jobs=$(sqlite3 "file:$data/production_queue.sqlite3?mode=ro" "SELECT count(*) FROM solid_queue_ready_executions")
fi

python3 -I - "$work/result.json" <<PY | tee -a "$RESULTS"
import json, sys
r = json.load(open(sys.argv[1]))
lat = r["latency"].get("200", {})
print(json.dumps({
  "run": "$run", "side": "$side", "label": "$label", "apps": $apps, "concurrency": $conc, "rate": $rate, "duration_s": $dur,
  "load_before": "$load_before", "io_full_avg10_before": float("$iopsi_before"),
  "batches_per_s": round(r["batches_per_s"], 2), "events_per_s": round(r["events_per_s"]),
  "events": int(r["events_per_s"] * r["duration_s"]), "posts": r["posts"], "status_counts": r["status_counts"],
  "p50_ms": lat.get("p50_ms"), "p90_ms": lat.get("p90_ms"), "p99_ms": lat.get("p99_ms"), "max_ms": lat.get("max_ms"),
  "cpu_load_s": round(($cpu1 - $cpu0) / 1e6, 1), "cpu_total_s": round(($cpu2 - $cpu0) / 1e6, 1), "tail_s": $t, "tail_cpu_s": round(($cpu2 - $cpu1) / 1e6, 1),
  "idle_anon_mib": round($idle_anon / 1048576), "idle_current_mib": round($idle_cur / 1048576),
  "peak_anon_mib": round($peak_anon / 1048576), "peak_current_mib": round($peak_cur / 1048576),
  "after_anon_mib": round($after_anon / 1048576),
  "telemetry_bytes": $disk, "written_bytes": $io_b1 - $io_b0, "write_ios": $io_n1 - $io_n0,
  "fsync": int("${fsync:-0}" or 0) if "${fsync:-0}".isdigit() else None,
  "fdatasync": int("${fdsync:-0}" or 0) if "${fdsync:-0}".isdigit() else None,
  "failed_jobs": $failed_jobs, "pending_jobs": $pending_jobs, "group_commit_txns": $gc_groups, "group_commit_batches": $gc_batches, "variant": "${VARIANT:-stock}",
}))
PY
cp "$work/load.txt" "$(dirname "$RESULTS")/$run.load.txt"
[ -n "${KEEP_SERVER:-}" ] || rvr_stop
