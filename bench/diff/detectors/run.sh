#!/usr/bin/env bash
# Detector diff: the same scripted telemetry timelines through railwatch-cloud
# (rw-baseline, rails_runner.rb under travel_to) and through the Rust ports
# (rw-detectors under time::travel_to), alert delivery pointed at one HTTPS
# capture webhook (receiver.py), then compare.py diffs issues, activities,
# alerts and deliveries per scenario.
#
#   bench/diff/detectors/run.sh [name,name,...]
#
# Nothing touches rw-baseline's own databases: each Rails scenario runs on
# its own primary file in /tmp/det/out (DATABASE_URL + establish_connection)
# and a throwaway tenant, and jobs go to an in-memory adapter, not Solid Queue.
set -euo pipefail
ONLY=${1:-}
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../.." && pwd)
W=$ROOT/bench/scratch/detectors
GW=172.17.0.1 RPORT=8796
mkdir -p "$W"
rm -rf "$W/rust" "$W/rails" "$W/received.jsonl"
cargo build -q --release -p railwatch-server --bin rw-detectors --manifest-path "$ROOT/Cargo.toml"
python3 -I "$HERE/scenarios.py" "$W/scenarios.json"

cd "$W"
if [ ! -f ca.pem ]; then
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 30 -subj "/CN=Railwatch detector diff CA" \
    -keyout ca.key -out ca.pem -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign" 2>/dev/null
  openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -subj "/CN=$GW" -keyout srv.key -out srv.csr 2>/dev/null
  printf "subjectAltName=IP:%s\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n" "$GW" > ext
  openssl x509 -req -in srv.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 30 -out srv.pem -extfile ext 2>/dev/null
fi
python3 -I "$HERE/receiver.py" "$GW" $RPORT srv.pem srv.key det-secret "$W/received.jsonl" > receiver.log 2>&1 &
RCV=$!; trap 'kill $RCV 2>/dev/null || true' EXIT
until grep -q listening receiver.log; do sleep 0.1; done
HOOK=https://$GW:$RPORT

# Rust.
RAILWATCH_ALLOW_PRIVATE_ALERT_TARGETS=1 RAILWATCH_ALERT_CA_FILE=$W/ca.pem \
  "$ROOT/target/release/rw-detectors" "$W/scenarios.json" "$W/rust" "$HOOK" $ONLY 2> rust.log | tail -1

# Rails.
docker exec rw-baseline sh -c 'rm -rf /tmp/det && mkdir -p /tmp/det/out'
docker cp "$W/scenarios.json" rw-baseline:/tmp/det/scenarios.json > /dev/null
docker cp "$W/ca.pem" rw-baseline:/tmp/det/ca.pem > /dev/null
docker cp "$HERE/rails_runner.rb" rw-baseline:/tmp/det/rails_runner.rb > /dev/null
docker exec -e RUN="$(date +%s)" -e HOOK="$HOOK" -e ONLY="$ONLY" -e SSL_CERT_FILE=/tmp/det/ca.pem \
  -e DATABASE_URL=sqlite3:/tmp/det/boot.sqlite3 rw-baseline bin/rails runner /tmp/det/rails_runner.rb > rails.log 2>&1 || { tail -30 rails.log; exit 1; }
grep "RAILS DONE" rails.log
mkdir -p "$W/rails"
docker cp rw-baseline:/tmp/det/out/. "$W/rails/" > /dev/null
docker exec rw-baseline rm -rf /tmp/det

python3 -I "$HERE/compare.py" "$W" ${VERBOSE:+--verbose} | tee "$W/diff.txt"
