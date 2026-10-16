#!/usr/bin/env bash
# Open-loop 20 batches/s for 40 s; 3 generation swaps 8 s apart under load.
set -u
RS=${RS:-$(cd "$(dirname "$0")/../.." && pwd)}
D=/dev/shm/rwr/handoff; rm -rf $D; mkdir -p $D
export RAILWATCH_DATA_DIR=$D RAILWATCH_RATE_LIMIT_ENV=0 RAILWATCH_RATE_LIMIT_IP=0
$RS/target/release/railwatch env create h1 | grep -o 'rw_[A-Za-z0-9]*' > $D/tokens
mkfifo $D/hold
python3 -I $(dirname $0)/handoff.py 8812 8 3 < $D/hold > $D/holder.out 2>&1 &
hp=$!; exec 9> $D/hold
until grep -q ready $D/holder.out; do sleep 0.05; done; until curl -sf 127.0.0.1:8812/up >/dev/null; do sleep 0.05; done
taskset -c 12-15 $RS/target/release/rw-load --url http://127.0.0.1:8812 --tokens $D/tokens --concurrency 16 --rate ${RATE:-20} --duration 40 > $D/load.out 2>&1
exec 9>&-; wait $hp
cat $D/holder.out; grep -E "posts in|batches/s|^ +(error|[0-9]{3}) " $D/load.out
echo "ingest_batches rows: $(sqlite3 $D/telemetry/h1.sqlite3 'select count(*) from ingest_batches')"
