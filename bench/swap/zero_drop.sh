#!/usr/bin/env bash
# The kamal-proxy run model under load: rw-load posts batches through
# kamal-proxy (TLS, like the gems) while railwatch-swap restarts the server
# SWAPS times, alternating binaries like an upgrade does. Proof of zero drop:
# every post answered 200, and the server's ingest_batches and
# ingest_receipts row counts equal the 200s. Exits 1 when any of that, or
# the number of completed swaps, is off.
#
#   bench/swap/zero_drop.sh [rate=20] [swaps=6] [duration=120]
#   FAIL_AT=3 bench/swap/zero_drop.sh ...   swap 3 upgrades to bad_upgrade.sh
#                                          and must roll back to the old binary
#
# Needs bench/swap/lab.sh up first. Uses only the lab's names.
set -euo pipefail
RATE=${1:-20} SWAPS=${2:-6} DURATION=${3:-120}
LAB=${LAB:-/var/tmp/rwswap/lab}
export RAILWATCH_SWAP_CONF=$LAB/opt/swap.conf
# shellcheck source=/dev/null
. "$RAILWATCH_SWAP_CONF"
SWAP=$LAB/opt/railwatch-swap
FAIL_AT=${FAIL_AT:-0}
HERE=$(cd "$(dirname "$0")" && pwd)
RUN=$(date +%H%M%S); OUT=$LAB/zero-drop-$RUN; mkdir -p "$OUT"; SLUG=rwswap-load-$RUN
# The load container goes with the script, however it ends.
trap 'docker rm -f "$SLUG" > /dev/null 2>&1 || true' EXIT

[ "$(cat "$ROOT/active" 2>/dev/null || echo none)" != none ] || "$SWAP" > /dev/null
c=$(cat "$ROOT/active")
docker exec "$NAME-$c" "/opt/bin/railwatch-$c" env create "$SLUG" --retention-days 30 > "$OUT/tokens"

# An "upgrade" alternates between two binaries (the same code, a different
# file each time, so bin/railwatch.new is really swapped in).
cp "$ROOT/bin/railwatch" "$OUT/railwatch.a"

EVERY=$(python3 -c "print(($DURATION - 15) / ($SWAPS + 1))")
# lab.sh put its CA beside the binary.
docker run --rm --name "$SLUG" --label rwswap-lab=1 --network host -v "$LAB/rw-load:/rw-load:ro" -v "$OUT":/out \
  -v "$ROOT/bin/ca.pem":/ca.pem:ro --add-host rs.rehearsal:127.0.0.1 rwswap-bookworm:amd64 \
  /rw-load --url https://rs.rehearsal:18443 --tokens /out/tokens --cacert /ca.pem \
  --concurrency 16 --rate "$RATE" --duration "$DURATION" --receipted --out /out/load.json > "$OUT/load.out" 2>&1 &
lp=$!
sleep 5
for i in $(seq "$SWAPS"); do
  sleep "$EVERY"
  if [ "$i" = "$FAIL_AT" ]; then
    cp "$HERE/bad_upgrade.sh" "$ROOT/bin/railwatch.new"
  else
    cp "$OUT/railwatch.a" "$ROOT/bin/railwatch.new"
  fi
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
read -r ok posts < <(python3 -c "import json; d=json.load(open('$OUT/load.json')); print(d['batches_ok'], d['posts'])")
completed=$(grep -c 'resumed' "$OUT/swaps.txt" || true)
rolled=$(grep -c 'ROLLED BACK' "$OUT/swaps.txt" || true)
aborted=$(grep -c 'ABORT' "$OUT/swaps.txt" || true)
echo "posts answered 200 / sent: $ok $posts"
echo "server: ingest_batches rows $batches, ingest_receipts $rows"
echo "swaps: $completed completed, $rolled rolled back, $aborted aborted; serving $NAME-$c"
grep -h "requests were held" "$OUT"/swap-*.log | sed -E 's/.*held ([0-9]+) ms; total ([0-9]+) ms.*/\1 \2/' | sort -n | awk '{h[NR]=$1; t[NR]=$2} END {print "held ms: min " h[1] ", max " h[NR] "; swap total ms max " t[NR]}'
echo "results in $OUT"

fail=0
check() { [ "$2" = "$3" ] || { echo "FAIL: $1: $2, expected $3"; fail=1; }; }
check "posts answered 200" "$ok" "$posts"
check "ingest_batches rows" "$batches" "$ok"
check "ingest_receipts rows" "$rows" "$ok"
if [ "$FAIL_AT" -ge 1 ] && [ "$FAIL_AT" -le "$SWAPS" ]; then
  check "completed swaps" "$completed" $(( SWAPS - 1 ))
  check "rolled back swaps" "$rolled" 1
  check "rejected upgrade set aside" "$([ -f "$ROOT/bin/railwatch.failed" ] && echo yes)" yes
else
  check "completed swaps" "$completed" "$SWAPS"
fi
# Whatever happened, the serving colour runs the good binary.
check "serving binary" "$(cmp -s "$ROOT/bin/railwatch-$c" "$OUT/railwatch.a" && echo good)" good
[ "$fail" = 0 ] && echo "PASS" || exit 1
