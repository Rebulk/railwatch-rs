#!/usr/bin/env bash
# The full matrix, REPS (default 3) repetitions, sides alternating first
# so slow drift on the host does not land on one side only. Every run is a
# fresh server on a fresh data dir (run.sh). Results: bench/results/rails-vs-rust/runs.jsonl
#
#   bench/rails-vs-rust/matrix.sh            # everything (~2 h)
#   ONLY="max-1app max-10app" bench/rails-vs-rust/matrix.sh
#
# label apps concurrency duration rate(batches/s, 0 = closed loop)
set -euo pipefail
cd "$(dirname "$0")"
CASES=(
  "max-1app    1 16 45 0"
  "max-10app  10 32 45 0"
  "max-50app  50 64 45 0"
  "rate-1app   1 16 45 8"
  "rate-10app 10 32 45 25"
  "rate-50app 50 64 45 30"
  "conc1-1app  1  1 30 0"
  "conc4-1app  1  4 30 0"
  "conc64-1app 1 64 30 0"
)
for rep in $(seq 1 "${REPS:-3}"); do
  for c in "${CASES[@]}"; do
    read -r label apps conc dur rate <<< "$c"
    if [ -n "${ONLY:-}" ] && [[ " $ONLY " != *" $label "* ]]; then continue; fi
    if [ $((rep % 2)) = 1 ]; then sides="rails rust"; else sides="rust rails"; fi
    for side in $sides; do
      echo "== rep $rep $side $label ($(date +%T), load $(cut -d' ' -f1-3 /proc/loadavg))" >&2
      SEED=$rep ./run.sh "$side" "$label" "$apps" "$conc" "$dur" "$rate" || echo "!! run failed: $side $label rep $rep" >&2
    done
  done
done
