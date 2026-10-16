#!/usr/bin/env bash
# Restart under load against the real binary: rw-load open loop at RATE
# batches/s for DURATION s while the server is swapped SWAPS times.
#
#   bench/restart/run.sh MODE [rate=40] [swaps=5] [duration=60] [port=8799]
#
# MODE:
#   bind     - no socket activation: SIGTERM, start a new process that binds
#              the port again (a plain `systemctl restart` without .socket).
#   restart  - socket held by holder.py (systemd .socket stand-in), stop then
#              start: what `systemctl restart` does with railwatch.socket (deploy/install.sh).
#   overlap  - socket held, new generation first, then stop the old one.
#
# Reports client-visible errors (rw-load's status table), the server-side
# ingest_batches row count against the client's 200s, and per-swap timings.
# Data on /dev/shm so only the restart is measured. Kills only its own PIDs.
set -u
MODE=${1:?usage: run.sh bind|restart|overlap [rate] [swaps] [duration] [port]}
RATE=${2:-40} SWAPS=${3:-5} DURATION=${4:-60} PORT=${5:-8799}
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
BIN=$ROOT/target/release/railwatch
D=/dev/shm/rw-gaps-restart-$MODE; rm -rf "$D"; mkdir -p "$D"
export RAILWATCH_DATA_DIR=$D RAILWATCH_RATE_LIMIT_ENV=0 RAILWATCH_RATE_LIMIT_IP=0 RAILWATCH_BIND=127.0.0.1:$PORT RAILWATCH_URL=http://127.0.0.1:$PORT
"$BIN" env create r1 | grep -o 'rw_[A-Za-z0-9]*' > "$D/tokens"
EVERY=$(python3 -c "print(($DURATION - 10) / ($SWAPS + 1))")

if [ "$MODE" = bind ]; then
  start() { "$BIN" serve >> "$D/server.log" 2>&1 & echo $!; }
  p=$(start); until curl -sf "127.0.0.1:$PORT/up" > /dev/null; do sleep 0.02; done
  "$ROOT/target/release/rw-load" --url "http://127.0.0.1:$PORT" --tokens "$D/tokens" --concurrency 16 --rate "$RATE" --duration "$DURATION" > "$D/load.out" 2>&1 &
  lp=$!
  for i in $(seq "$SWAPS"); do
    sleep "$EVERY"
    t0=$(date +%s%N); kill -TERM "$p"; while kill -0 "$p" 2>/dev/null; do sleep 0.005; done
    p=$(start); until curl -sf "127.0.0.1:$PORT/up" > /dev/null; do sleep 0.005; done
    echo "swap $i: port refused connections for $(( ($(date +%s%N) - t0) / 1000000 )) ms (stop + start + bind)"
  done
  wait "$lp"; kill -TERM "$p"; wait "$p" 2>/dev/null
else
  mkfifo "$D/hold"
  python3 -I "$HERE/holder.py" "$BIN" "$PORT" "$EVERY" "$SWAPS" "$MODE" "$D" < "$D/hold" > "$D/holder.out" 2>&1 &
  hp=$!; exec 9> "$D/hold"
  until grep -q ready "$D/holder.out"; do sleep 0.05; done
  "$ROOT/target/release/rw-load" --url "http://127.0.0.1:$PORT" --tokens "$D/tokens" --concurrency 16 --rate "$RATE" --duration "$DURATION" > "$D/load.out" 2>&1
  exec 9>&-; wait "$hp"
  grep swap "$D/holder.out"
fi
grep -E "posts in|batches/s|^ +(error|[0-9]{3}) " "$D/load.out"
echo "server ingest_batches rows: $(sqlite3 "$D/telemetry/r1.sqlite3" 'select count(*) from ingest_batches')"
grep -c "socket=\"inherited (LISTEN_FDS)\"\|inherited (LISTEN_FDS)" "$D/server.log" | sed 's/^/generations that inherited the socket: /'
