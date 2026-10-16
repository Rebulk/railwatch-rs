#!/usr/bin/env bash
# The kamal-proxy run model under load: rw-load posts batches through
# kamal-proxy (TLS, like the gems) while railwatch-swap restarts the server
# SWAPS times, alternating binaries like an upgrade does. Proof of zero drop:
# every post answered 200, and the server's ingest_batches row count equals
# the 200s.
#
#   bench/swap/zero_drop.sh [rate=20] [swaps=6] [duration=120]
#
# Needs bench/swap/lab.sh up first. Uses only the lab's names.
set -euo pipefail
RATE=${1:-20} SWAPS=${2:-6} DURATION=${3:-120}
LAB=${LAB:-/var/tmp/rwswap/lab}
export RAILWATCH_SWAP_CONF=$LAB/opt/swap.conf
# shellcheck source=/dev/null
. "$RAILWATCH_SWAP_CONF"
SWAP=$LAB/opt/railwatch-swap
RUN=$(date +%H%M%S); OUT=$LAB/zero-drop-$RUN; mkdir -p "$OUT"; SLUG=rwswap-load-$RUN

[ "$(cat "$ROOT/active" 2>/dev/null || echo none)" != none ] || "$SWAP" > /dev/null
c=$(cat "$ROOT/active")
docker exec "$NAME-$c" /opt/bin/railwatch env create "$SLUG" --retention-days 30 > "$OUT/tokens"

# An "upgrade" alternates between two binaries (the same code, a different
# file each time, so bin/railwatch.new is really swapped in).
cp "$ROOT/bin/railwatch" "$OUT/railwatch.a"

EVERY=$(python3 -c "print(($DURATION - 15) / ($SWAPS + 1))")
docker run --rm --network host -v "$LAB/rw-load:/rw-load:ro" -v "$OUT":/out -v /tmp/rwswap-tls:/certs:ro \
  --add-host rs.rehearsal:127.0.0.1 rwswap-bookworm:amd64 \
  sh -c "cp /certs/ca.pem /tmp/ca.pem; /rw-load --url https://rs.rehearsal:18443 --tokens /out/tokens --cacert /tmp/ca.pem \
         --concurrency 16 --rate $RATE --duration $DURATION --receipted --out /out/load.json" > "$OUT/load.out" 2>&1 &
lp=$!
sleep 5
for i in $(seq "$SWAPS"); do
  sleep "$EVERY"
  cp "$OUT/railwatch.a" "$ROOT/bin/railwatch.new"
  "$SWAP" > "$OUT/swap-$i.log" 2>&1 || true
  sed "s/^/swap $i: /" "$OUT/swap-$i.log" >> "$OUT/swaps.txt"
  grep -E '^[0-9:.]+ (5\.|ABORT|ROLLED)' "$OUT/swap-$i.log" | sed "s/^/swap $i: /"
done
wait $lp
c=$(cat "$ROOT/active")
sleep 2
# Counted after the run, through the server's own files (read-only opens).
counts=$(docker run --rm --network none -v "$VOLUME":/data -e SLUG="$SLUG" rwswap-sqlite sh -c '
  b=0; for f in /data/telemetry/$SLUG/core/*.sqlite3; do b=$((b + $(sqlite3 "file:$f?mode=ro" "SELECT count(*) FROM ingest_batches"))); done
  echo "$b $(sqlite3 "file:/data/telemetry/$SLUG/meta.sqlite3?mode=ro" "SELECT count(*) FROM ingest_receipts")"')
batches=${counts% *}; rows=${counts#* }
grep -E "posts in|batches/s|^ +(error|[0-9]{3}) " "$OUT/load.out"
ok=$(python3 -c "import json; d=json.load(open('$OUT/load.json')); print(d['batches_ok'], d['posts'])")
echo "posts answered 200 / sent: $ok"
echo "server: ingest_batches rows $batches, ingest_receipts $rows"
echo "swaps: $(grep -c 'resumed' "$OUT/swaps.txt") completed, $(grep -cE 'ABORT|ROLLED' "$OUT/swaps.txt") aborted; serving $NAME-$c"
grep -h "requests were held" "$OUT"/swap-*.log | sed -E 's/.*held ([0-9]+) ms; total ([0-9]+) ms.*/\1 \2/' | sort -n | awk '{h[NR]=$1; t[NR]=$2} END {print "held ms: min " h[1] ", max " h[NR] "; swap total ms max " t[NR]}'
echo "results in $OUT"
