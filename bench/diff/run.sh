#!/usr/bin/env bash
# Differential test: the same bytes to Rails (rw-baseline, :8780) and to the
# Rust server (:8790, fresh data dir), then compare every ack and every
# table of both tenant files, row by row, ids included.
#
#   bench/diff/run.sh [batches=20] [records=200] [seed=7]
#
# Rails side: a fresh scratch environment `diff-<run>` in the bench
# application (its own tenant file; env-0001 and its tokens are untouched).
# Rust side: the same token digest imported with `railwatch env import`.
# Rails' jobs container keeps running, so rollups/release_health on the Rails
# side are compared for nothing (phase 2); see compare.py.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
BATCHES=${1:-20} RECORDS=${2:-200} SEED=${3:-7}
RUN=$(date +%s)
WORK=${WORK:-/tmp/rw-diff-$RUN}
RUST_PORT=${RUST_PORT:-8790}
FIXTURES=$ROOT/crates/server/tests/fixtures/wire_fixtures.json
mkdir -p "$WORK"
echo "work dir $WORK"

cargo build -q --release -p railwatch-server --manifest-path "$ROOT/Cargo.toml"
BIN=$ROOT/target/release/railwatch

# 1. Rails scratch environment (business plan, 90 days, quota not reachable).
SLUG_NAME=diff-$RUN
docker exec -i -e NAME="$SLUG_NAME" rw-baseline bin/rails runner - > "$WORK/rails-env.txt" <<'RUBY'
account = Account.find_or_initialize_by(slug: "bench")
account.update!(name: "bench", plan: "business", monthly_event_quota: 1_000_000_000_000)
app = account.applications.find_or_create_by!(slug: "bench") { |a| a.name = "bench" }
env = app.environments.create!(name: ENV.fetch("NAME"))
abort "no tenant" unless env.telemetry_exists?
puts "TOKEN #{env.token}"
puts "SLUG #{env.slug}"
puts "RETENTION #{env.retention_days}"
RUBY
TOKEN=$(sed -n 's/^TOKEN //p' "$WORK/rails-env.txt")
SLUG=$(sed -n 's/^SLUG //p' "$WORK/rails-env.txt")
RETENTION=$(sed -n 's/^RETENTION //p' "$WORK/rails-env.txt")
DIGEST=$(printf %s "$TOKEN" | sha256sum | cut -d' ' -f1)
echo "rails env $SLUG (retention ${RETENTION}d)"

# 2. Rust server with a fresh data dir and the same digest.
export RAILWATCH_DATA_DIR=$WORK/rust-data
"$BIN" env import "$SLUG" "$DIGEST" --retention-days "$RETENTION" >/dev/null
# LAYOUT=single: replace the per-day directory new environments get with a
# single file, so the comparison is byte-level including FTS5's segments
# (export rebuilds logs_fts, whose internal blobs then differ from Rails').
if [ "${LAYOUT:-}" = single ]; then
  "$BIN" export "$SLUG" "$WORK/empty.sqlite3" >/dev/null
  rm -rf "$RAILWATCH_DATA_DIR/telemetry/$SLUG"
  mv "$WORK/empty.sqlite3" "$RAILWATCH_DATA_DIR/telemetry/$SLUG.sqlite3"
fi
RAILWATCH_BIND=127.0.0.1:$RUST_PORT RAILWATCH_TRUST_PROXY=1 "$BIN" serve > "$WORK/rust.log" 2>&1 &
RUST_PID=$!
trap 'kill $RUST_PID 2>/dev/null || true' EXIT
for _ in $(seq 1 50); do curl -fsS -o /dev/null "http://127.0.0.1:$RUST_PORT/up" && break; sleep 0.1; done

# 3. Batches, then the same posts in the same order to both. A distinct XFF
#    per run keeps Rails' per-IP limiter bucket clean.
python3 -I "$HERE/gen_batches.py" "$FIXTURES" "$WORK/batches" "$BATCHES" "$RECORDS" "$SEED"
XFF=192.0.2.$((RUN % 250 + 1))
python3 -I "$HERE/post.py" "http://127.0.0.1:8780/ingest" "$TOKEN" "$WORK/batches" "$WORK/acks-rails.json" "$XFF"
python3 -I "$HERE/post.py" "http://127.0.0.1:$RUST_PORT/ingest" "$TOKEN" "$WORK/batches" "$WORK/acks-rust.json" "$XFF"

# 4. Acks.
python3 -I - "$WORK/acks-rails.json" "$WORK/acks-rust.json" <<'PY'
import json, sys
a, b = json.load(open(sys.argv[1])), json.load(open(sys.argv[2]))
diff = [(x, y) for x, y in zip(a, b) if (x["status"], x["ack"], x["retry_after"]) != (y["status"], y["ack"], y["retry_after"])]
print(f"acks: {len(a) - len(diff)}/{len(a)} identical (status + JSON body + Retry-After)")
for x, y in diff[:10]:
    print(f"  post {x['post']} {x['file']}{' replay' if x['replay'] else ''}:\n    rails {x['status']} {json.dumps(x['ack'])[:400]}\n    rust  {y['status']} {json.dumps(y['ack'])[:400]}")
sys.exit(1 if diff else 0)
PY
ACKS=$?

# 5. Tenant files: stop Rust (drains its writer), copy Rails' file out with
#    sqlite3's online backup so the WAL is folded in consistently.
kill $RUST_PID; wait $RUST_PID 2>/dev/null || true; trap - EXIT
docker exec rw-baseline sqlite3 "/rails/storage/telemetry/$SLUG.sqlite3" ".backup /tmp/$SLUG.copy.sqlite3"
docker cp "rw-baseline:/tmp/$SLUG.copy.sqlite3" "$WORK/rails.sqlite3" >/dev/null
docker exec rw-baseline rm -f "/tmp/$SLUG.copy.sqlite3"
# New environments are per-day directories: export joins them back into one
# file (storage::export); a single-file tenant is copied as before.
if [ -d "$RAILWATCH_DATA_DIR/telemetry/$SLUG" ]; then
  "$BIN" export "$SLUG" "$WORK/rust.sqlite3" >/dev/null
else
  sqlite3 "$RAILWATCH_DATA_DIR/telemetry/$SLUG.sqlite3" ".backup $WORK/rust.sqlite3"
fi
python3 -I "$HERE/compare.py" "$WORK/rails.sqlite3" "$WORK/rust.sqlite3"
echo "rails: $WORK/rails.sqlite3  rust: $WORK/rust.sqlite3  acks: $WORK/acks-*.json"
exit $ACKS
