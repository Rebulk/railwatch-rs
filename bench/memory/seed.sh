#!/usr/bin/env bash
# Builds the data dir every bench/memory/run.sh run starts from: one account,
# 6 environments (production has 6 tenants), DAYS days of history from rw-load
# --spread-days, signed in through the trusted proxy (dash.py).
#
#   bench/memory/seed.sh OUT_DIR [BATCHES=6000] [DAYS=14]
#
# BIN_DIR picks the railwatch + rw-load binaries (default target/release).
# 6000 batches x 500 records = 3M rows, ~2.5 GB on disk. Takes a few minutes:
# the server is stopped only once the rollups have caught up (CPU idle).
set -euo pipefail
OUT=${1:?usage: seed.sh OUT_DIR [BATCHES] [DAYS]} BATCHES=${2:-6000} DAYS=${3:-14}
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
BIN=${BIN_DIR:-$ROOT/target/release}
PORT=${PORT:-18732}
[ -e "$OUT" ] && { echo "seed.sh: $OUT exists" >&2; exit 1; }
mkdir -p "$OUT"
export RAILWATCH_DATA_DIR=$OUT RAILWATCH_TRUST_PROXY=1 RAILWATCH_RATE_LIMIT_ENV=0 RAILWATCH_RATE_LIMIT_IP=0 PORT
head -c 48 /dev/urandom | base64 -w0 > "$OUT/proxy.secret"
chmod 600 "$OUT/proxy.secret"
echo 'bench-password-for-memory-runs' | "$BIN/railwatch" setup --email bench@example.com --name Bench --account Bench --app shop --env e1 --password-stdin \
  | grep -o 'rw_[A-Za-z0-9]*' > "$OUT/tokens"
for i in 2 3 4 5 6; do "$BIN/railwatch" env create "e$i" >> "$OUT/tokens"; done
# `env create` puts environments in a "default" account; the proxied user
# belongs to the first account only, so its application moves there.
sqlite3 "$OUT/control.sqlite3" "UPDATE applications SET account_id = 1 WHERE slug = 'default'"
if ss -ltn | grep -q "127.0.0.1:$PORT "; then echo "port $PORT already has a listener" >&2; exit 1; fi
"$BIN/railwatch" serve > "$OUT/seed-server.log" 2>&1 &
pid=$!
trap 'kill $pid 2>/dev/null || true' EXIT
until curl -sf "127.0.0.1:$PORT/up" > /dev/null; do kill -0 $pid 2> /dev/null || { echo "server exited (port $PORT in use?)" >&2; exit 1; }; sleep 0.2; done
"$BIN/rw-load" --url "http://127.0.0.1:$PORT" --tokens "$OUT/tokens" --concurrency 12 --batches "$BATCHES" --duration 3600 \
  --spread-days "$DAYS" --xff-rotate 64 --seed 7 2>&1 | grep -E 'batches/s|^ +[0-9]{3} '
# Wait for the rollups to drain: 3 consecutive 5 s windows under 0.2 CPU s.
quiet=0
while [ $quiet -lt 3 ]; do
  a=$(awk '{print $14+$15}' /proc/$pid/stat); sleep 5; b=$(awk '{print $14+$15}' /proc/$pid/stat)
  if [ $((b - a)) -lt 20 ]; then quiet=$((quiet + 1)); else quiet=0; fi
done
kill -TERM $pid; wait $pid || true
trap - EXIT
sqlite3 "$OUT/control.sqlite3" "SELECT a.id || ' ' || e.id || ' ' || e.slug FROM environments e JOIN applications a ON a.id = e.application_id ORDER BY e.id" > "$OUT/envs"
echo "seeded $OUT: $(du -sh "$OUT/telemetry" | cut -f1) in $(find "$OUT/telemetry" -name '*.sqlite3' | wc -l) files"
