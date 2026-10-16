#!/usr/bin/env bash
# Stock binary, open-loop 20 batches/s for 30 s; at t=10 s SIGTERM + start a new process
# on the same port (what `systemctl restart` or a Kamal-less deploy does). Counts what
# the client saw. Data on /dev/shm so only the restart is measured.
set -u
RS=${RS:-$(cd "$(dirname "$0")/../.." && pwd)}
D=/dev/shm/rwr/restart; rm -rf $D; mkdir -p $D
export RAILWATCH_DATA_DIR=$D RAILWATCH_RATE_LIMIT_ENV=0 RAILWATCH_RATE_LIMIT_IP=0 PORT=8811
$RS/target/release/railwatch env create r1 | grep -o 'rw_[A-Za-z0-9]*' > $D/tokens
start() { taskset -c 8-11 $RS/target/release/railwatch serve >> $D/server.log 2>&1 & echo $!; }
p=$(start); until curl -sf 127.0.0.1:$PORT/up >/dev/null; do sleep 0.05; done
taskset -c 12-15 $RS/target/release/rw-load --url http://127.0.0.1:$PORT --tokens $D/tokens --concurrency 16 --rate 20 --duration 30 > $D/load.out 2>&1 &
lp=$!
sleep 10
t0=$(date +%s%N); kill -TERM $p; while kill -0 $p 2>/dev/null; do sleep 0.01; done
p=$(start); until curl -sf 127.0.0.1:$PORT/up >/dev/null; do sleep 0.01; done
echo "listener gap: $(( ($(date +%s%N)-t0)/1000000 )) ms"
wait $lp; kill -TERM $p; wait $p 2>/dev/null
grep -E "posts in|batches/s|^ +(error|[0-9]{3}) " $D/load.out
echo "rows in queries: $(sqlite3 $D/telemetry/r1.sqlite3 'select count(*) from ingest_batches')  ingest_batches"
