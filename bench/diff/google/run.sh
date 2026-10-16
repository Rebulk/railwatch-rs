#!/usr/bin/env bash
# Google sign-in, Rails vs Rust, on the same flows against the same mock
# Google (mock_google.py): cases.json through railwatch-cloud in rw-baseline
# (rails_flows.rb, scratch primary DB /tmp/rwcmp.sqlite3 in the container,
# the baseline's own data untouched) and through a fresh railwatch-rs
# (rust_flows.py), then compare.py.
#
#   bench/diff/google/run.sh        # writes last-diff.txt
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../.." && pwd)
WORK=$(mktemp -d /tmp/rw-google-diff.XXXX)
MOCK_PORT=${MOCK_PORT:-8798} RUST_PORT=${RUST_PORT:-8799}
CLIENT_ID=railwatch-test.apps.googleusercontent.com CLIENT_SECRET=test-secret

if curl -fs -o /dev/null "http://127.0.0.1:$MOCK_PORT/certs"; then
  echo "port $MOCK_PORT is taken (a stale mock?); stop it first" >&2
  exit 1
fi
python3 -I "$HERE/mock_google.py" "$MOCK_PORT" & MOCK_PID=$!
trap 'kill $MOCK_PID ${RUST_PID:-} 2>/dev/null || true' EXIT
until curl -fs -o /dev/null "http://127.0.0.1:$MOCK_PORT/certs"; do sleep 0.2; done

# Rails. The scratch DB gets the schema once.
docker exec rw-baseline test -f /tmp/rwcmp.sqlite3 || docker exec -e DATABASE_URL=sqlite3:/tmp/rwcmp.sqlite3 \
  -e DISABLE_DATABASE_ENVIRONMENT_CHECK=1 rw-baseline bin/rails db:schema:load:primary > /dev/null
docker cp "$HERE/rails_flows.rb" rw-baseline:/tmp/rails_flows.rb
docker cp "$HERE/cases.json" rw-baseline:/tmp/google_cases.json
docker exec -e DATABASE_URL=sqlite3:/tmp/rwcmp.sqlite3 -e GOOGLE_CLIENT_ID=$CLIENT_ID -e GOOGLE_CLIENT_SECRET=$CLIENT_SECRET \
  -e MOCK_GOOGLE="http://172.17.0.1:$MOCK_PORT" -e CASES=/tmp/google_cases.json -e RAILS_LOG_LEVEL=error \
  rw-baseline bin/rails runner /tmp/rails_flows.rb 2>/dev/null | grep '^{"case"' > "$WORK/rails.jsonl"

# Rust.
cargo build -q -p railwatch-server --manifest-path "$ROOT/Cargo.toml"
RAILWATCH_DATA_DIR="$WORK/data" RAILWATCH_BIND=127.0.0.1:$RUST_PORT RAILWATCH_SECRET_KEY_BASE=$(printf '0%.0s' {1..128}) \
  RAILWATCH_GOOGLE_CLIENT_ID=$CLIENT_ID RAILWATCH_GOOGLE_CLIENT_SECRET=$CLIENT_SECRET \
  RAILWATCH_GOOGLE_ISSUER="http://127.0.0.1:$MOCK_PORT" RAILWATCH_SIGNUP=open RAILWATCH_LOG=error \
  "$ROOT/target/debug/railwatch" serve > "$WORK/rust.log" 2>&1 & RUST_PID=$!
until curl -fs -o /dev/null "http://127.0.0.1:$RUST_PORT/up"; do sleep 0.2; done
python3 -I "$HERE/rust_flows.py" "http://127.0.0.1:$RUST_PORT" "http://127.0.0.1:$MOCK_PORT" "$WORK/data" "$HERE/cases.json" > "$WORK/rust.jsonl"

python3 -I "$HERE/compare.py" "$WORK/rails.jsonl" "$WORK/rust.jsonl" | tee "$HERE/last-diff.txt"
