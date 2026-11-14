#!/usr/bin/env bash
# Mechanism runs for "concurrent writes to one app", after matrix.sh:
#
# 1. Rust, 1 app, concurrency 16 and 64, with the writer's debug log on:
#    every "transaction committed batches=N" line is one SQLite transaction,
#    so batches per transaction is measured, not assumed. (Rails' is 1 by
#    construction: Ingest::Batch#write! opens one transaction per request.)
# 2. Rust, 1 app, concurrency 16, PRAGMA synchronous=OFF
#    (RAILWATCH_SQLITE_SYNC=off, a bench-only knob): if throughput does not
#    move, fsync is not what limits it.
# Same run.sh, same slice and disk; variant tags in runs.jsonl.
set -euo pipefail
cd "$(dirname "$0")"
. ./lib.sh
LOG=$RVR_ROOT/bench/results/rails-vs-rust/mechanism.txt
: > "$LOG"
for rep in 1 2 3; do
  for conc in 16 64; do
    VARIANT=debug-log RUST_EXTRA_ENV="-e RAILWATCH_LOG=info,railwatch_server::tenants=debug" KEEP_SERVER=1 SEED=$rep \
      ./run.sh rust "mech-conc$conc-1app" 1 "$conc" 30 0 > /dev/null
    docker logs rvr-rust 2>&1 | sed 's/\x1b\[[0-9;]*m//g' | grep 'transaction committed' | sed -n 's/.*batches=\([0-9]*\).*/\1/p' \
      | awk -v c="$conc" -v r="$rep" '{n++; b+=$1; if ($1>m) m=$1} END {printf "rust conc %d rep %d: %d transactions, %d batches, %.2f batches/txn, max %d\n", c, r, n, b, b/n, m}' | tee -a "$LOG"
    rvr_stop
  done
  VARIANT=sync-off RUST_EXTRA_ENV="-e RAILWATCH_SQLITE_SYNC=off" SEED=$rep ./run.sh rust "mech-syncoff-conc16-1app" 1 16 30 0 | tail -1 \
    | python3 -I -c 'import json,sys; d=json.loads(sys.stdin.read()); print("rust synchronous=off conc 16 rep", sys.argv[1], ":", d["events_per_s"], "ev/s p50", round(d["p50_ms"]), "fsync", d["fsync"])' "$rep" | tee -a "$LOG"
done
# 3. Rails, the same two diagnostics' counterpart: synchronous=OFF on every
#    connection (rails/zz_bench_sync_off.rb), 1 app, concurrency 16.
for rep in 1 2 3; do
  VARIANT=sync-off RAILS_EXTRA_MOUNTS="-v $PWD/rails/zz_bench_sync_off.rb:/rails/config/initializers/zz_bench_sync_off.rb:ro" SEED=$rep \
    ./run.sh rails "mech-syncoff-conc16-1app" 1 16 30 0 | tail -1 \
    | python3 -I -c 'import json,sys; d=json.loads(sys.stdin.read()); print("rails synchronous=off conc 16 rep", sys.argv[1], ":", d["events_per_s"], "ev/s p50", round(d["p50_ms"]), "fsync", d["fsync"])' "$rep" | tee -a "$LOG"
done
