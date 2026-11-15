#!/usr/bin/env bash
# The counterfactual: the same Rails production image with ONE change, a
# per-tenant group commit (rails/zz_bench_group_commit.rb, mounted as an
# initializer), through the same run.sh. Results land in the same runs.jsonl
# with "variant": "group-commit".
#
#   bench/rails-vs-rust/counterfactual.sh
set -euo pipefail
cd "$(dirname "$0")"
failed=0
export VARIANT=group-commit
export RAILS_EXTRA_MOUNTS="-v $PWD/rails/zz_bench_group_commit.rb:/rails/config/initializers/zz_bench_group_commit.rb:ro"
CASES=(
  "cf-max-1app    1 16 45 0"
  "cf-max-10app  10 32 45 0"
  "cf-max-50app  50 64 45 0"
  "cf-rate-10app 10 32 45 25"
  "cf-conc64-1app 1 64 30 0"
)
for rep in $(seq 1 "${REPS:-3}"); do
  for c in "${CASES[@]}"; do
    read -r label apps conc dur rate <<< "$c"
    if [ -n "${ONLY:-}" ] && [[ " $ONLY " != *" $label "* ]]; then continue; fi
    echo "== rep $rep rails+group-commit $label ($(date +%T), load $(cut -d' ' -f1-3 /proc/loadavg))" >&2
    SEED=$rep ./run.sh rails "$label" "$apps" "$conc" "$dur" "$rate" || { echo "!! run failed: $label rep $rep" >&2; failed=$((failed + 1)); }
  done
done
[ "$failed" = 0 ] || { echo "$failed runs failed" >&2; exit 1; }
