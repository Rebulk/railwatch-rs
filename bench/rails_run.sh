#!/usr/bin/env bash
# One closed-loop rw-load run against the Rails baseline (rw-baseline :8780).
#   bench/rails_run.sh <label> <envs> <concurrency> <duration>
# Seeds <envs> fresh environments (rotates tokens), rotates X-Forwarded-For so
# the per-IP limiter runs but never trips, samples memory of BOTH containers
# (web + jobs, cgroup anon+file) every 0.5 s, and reports the peaks.
set -euo pipefail
cd "$(dirname "$0")/.."
label=$1 envs=$2 conc=$3 dur=$4
mkdir -p bench/scratch; D=$(mktemp -d "$PWD/bench/scratch/rails-$label.XXXX")
bench/rails/seed.sh "$envs" > /dev/null
cp "bench/rails/tokens-$envs.txt" "$D/tokens"
docker update --cpuset-cpus "${SERVER_CPUS:-8-11}" rw-baseline rw-baseline-jobs > /dev/null
cg() { echo "/sys/fs/cgroup/system.slice/docker-$(docker inspect -f '{{.Id}}' "$1").scope"; }
web=$(cg rw-baseline) jobs=$(cg rw-baseline-jobs)
mem() { awk '/^anon /{print $2}' "$1/memory.stat"; }  # private memory; page cache of the tenant files excluded
cpu() { awk '/^usage_usec/{print $2}' "$1/cpu.stat"; }
w0=$(cpu "$web") j0=$(cpu "$jobs")
( while true; do echo "$(mem "$web") $(mem "$jobs")"; sleep 0.5; done ) > "$D/mem" &
sampler=$!
trap 'kill $sampler 2>/dev/null || true' EXIT
taskset -c "${LOAD_CPUS:-12-15}" ./target/release/rw-load --url http://127.0.0.1:8780 --tokens "$D/tokens" --concurrency "$conc" --rate "${RATE:-0}" \
  --duration "$dur" --xff-rotate 256 --out "$D/result.json" 2>&1 | grep -E 'batches/s|^ +[0-9]{3} ' || true
w1=$(cpu "$web") j1=$(cpu "$jobs")
kill $sampler
pw=$(awk '{print $1}' "$D/mem" | sort -n | tail -1); pj=$(awk '{print $2}' "$D/mem" | sort -n | tail -1)
echo "== $label: peak anon web $((pw/1048576)) MiB + jobs $((pj/1048576)) MiB; CPU s web $(( (w1-w0)/1000000 )) jobs $(( (j1-j0)/1000000 )); dir $D"
