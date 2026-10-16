#!/usr/bin/env bash
# Props-diff: the dashboard props Rails renders vs what the Rust dashboard
# module computes, over the SAME tenant file at the SAME frozen "now".
#
#   bench/diff/props/run.sh [executions=6000] [spread_days=9]
#   REUSE=/tmp/rw-props-NNN bench/diff/props/run.sh   # skip build+ingest, re-capture and re-diff
#
# 1. A scratch environment `props-<run>` in the bench application on
#    rw-baseline (:8780); env-0001 and its token are untouched.
# 2. gen_tenant.py batches (spread over days) and deploys posted to Rails.
# 3. settle_rails.rb: RollupJob / ReleaseHealthRollupJob / exception grouping
#    run synchronously for every hour, so nothing races the jobs container.
# 4. The tenant file is copied out of the container: Rust reads Rails' own
#    rows, rollups included (Phase 1 proved ingest row-identical; using the
#    same file removes that variable entirely).
# 5. capture_rails.rb GETs every case as an Inertia XHR with travel_to(NOW);
#    props-dump computes the same cases at NOW over the copied file.
# 6. compare.py diffs them.
#
# SPLIT=1: props-dump reads the copied file after `railwatch split`, i.e.
# through the per-day layout's union reader (docs/STORAGE.md).
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../.." && pwd)
EXECUTIONS=${1:-6000} SPREAD=${2:-9}
FIXTURES=$ROOT/crates/server/tests/fixtures/wire_fixtures.json
cargo build -q --release -p railwatch-server --bin props-dump --bin railwatch --manifest-path "$ROOT/Cargo.toml"
DUMP=$ROOT/target/release/props-dump

if [ -n "${REUSE:-}" ]; then
  WORK=$REUSE
  SLUG=$(cat "$WORK/slug") ENV_ID=$(cat "$WORK/env_id") NOW=$(cat "$WORK/now")
else
  RUN=$(date +%s)
  WORK=${WORK:-/tmp/rw-props-$RUN}
  mkdir -p "$WORK"
  # NOW: a whole-second instant a minute ago, so every record is in the past.
  NOW=$((RUN - 60))
  echo "$NOW" > "$WORK/now"
  docker exec -i -e NAME="props-$RUN" rw-baseline bin/rails runner - > "$WORK/env.txt" 2>/dev/null <<'RUBY'
account = Account.find_or_initialize_by(slug: "bench")
account.update!(name: "bench", plan: "business", monthly_event_quota: 1_000_000_000_000)
app = account.applications.find_or_create_by!(slug: "bench") { |a| a.name = "bench" }
env = app.environments.create!(name: ENV.fetch("NAME"))
puts "TOKEN #{env.token}"
puts "SLUG #{env.slug}"
puts "ENV_ID #{env.id}"
RUBY
  TOKEN=$(sed -n 's/^TOKEN //p' "$WORK/env.txt")
  SLUG=$(sed -n 's/^SLUG //p' "$WORK/env.txt"); echo "$SLUG" > "$WORK/slug"
  ENV_ID=$(sed -n 's/^ENV_ID //p' "$WORK/env.txt"); echo "$ENV_ID" > "$WORK/env_id"
  echo "rails env $SLUG (id $ENV_ID), now=$NOW, work $WORK"
  python3 -I "$HERE/gen_tenant.py" "$FIXTURES" "$WORK/batches" --now "$NOW" --spread-days "$SPREAD" --executions "$EXECUTIONS"
  python3 -I "$ROOT/bench/diff/post.py" "http://127.0.0.1:8780/ingest" "$TOKEN" "$WORK/batches" "$WORK/acks.json" "192.0.2.$((RUN % 250 + 1))"
  python3 -I - "$WORK/batches/deploys.json" "$TOKEN" <<'PY'
import json, sys, urllib.request
for d in json.load(open(sys.argv[1])):
    req = urllib.request.Request("http://127.0.0.1:8780/ingest/deploys", data=json.dumps(d).encode(), method="POST",
        headers={"Authorization": f"Bearer {sys.argv[2]}", "Content-Type": "application/json"})
    urllib.request.urlopen(req).read()
PY
  docker cp "$HERE/settle_rails.rb" rw-baseline:/tmp/settle_rails.rb
  docker exec -e SLUG="$SLUG" rw-baseline bin/rails runner /tmp/settle_rails.rb 2>/dev/null | grep SETTLED
fi

# Copy the tenant file out (checkpoint first so the copy is self-contained).
docker exec -i -e SLUG="$SLUG" rw-baseline bin/rails runner - >/dev/null 2>&1 <<'RUBY'
Environment.find_by!(slug: ENV["SLUG"]).with_telemetry { Railwatch::TelemetryRecord.connection.execute("PRAGMA wal_checkpoint(TRUNCATE)") }
RUBY
docker cp "rw-baseline:/rails/storage/telemetry/$SLUG.sqlite3" "$WORK/tenant.sqlite3"
rm -f "$WORK/tenant.sqlite3-wal" "$WORK/tenant.sqlite3-shm"

# Control rows first (cases pick a deploy from them), then cases, then capture.
echo '[]' > "$WORK/cases.json"
docker cp "$WORK/cases.json" rw-baseline:/tmp/props-cases.json
docker cp "$HERE/capture_rails.rb" rw-baseline:/tmp/capture_rails.rb
docker exec -e SLUG="$SLUG" -e NOW="$NOW" -e WARM=0 rw-baseline bin/rails runner /tmp/capture_rails.rb 2>/dev/null | grep CAPTURED >/dev/null
docker cp rw-baseline:/tmp/props-control.json "$WORK/control.json"
python3 -I "$HERE/make_cases.py" "$WORK/tenant.sqlite3" "$WORK/control.json" "$WORK/cases.json" ${QUICK:+--quick}
# The custom window: the 6 hours ending 2 hours before NOW.
python3 -I - "$WORK/cases.json" "$NOW" <<'PY'
import json, sys, datetime
p, now = sys.argv[1], int(sys.argv[2])
iso = lambda t: datetime.datetime.fromtimestamp(t, datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
cases = json.load(open(p))
for c in cases:
    for k, v in list(c["params"].items()):
        c["params"][k] = v.replace("CUSTOM_FROM", iso(now - 8 * 3600)).replace("CUSTOM_TO", iso(now - 2 * 3600))
json.dump(cases, open(p, "w"), indent=1)
PY
docker cp "$WORK/cases.json" rw-baseline:/tmp/props-cases.json
docker exec -e SLUG="$SLUG" -e NOW="$NOW" -e WARM="${WARM:-0}" rw-baseline bin/rails runner /tmp/capture_rails.rb 2>/dev/null | grep CAPTURED
docker cp rw-baseline:/tmp/props-rails.json "$WORK/rails.json"
docker cp rw-baseline:/tmp/props-rails-timings.json "$WORK/rails.timings.json"

TENANT=$WORK/tenant.sqlite3
if [ -n "${SPLIT:-}" ]; then
  rm -rf "$WORK/split"; mkdir -p "$WORK/split/telemetry"
  sqlite3 "$WORK/tenant.sqlite3" ".backup $WORK/split/telemetry/t.sqlite3"
  RAILWATCH_DATA_DIR=$WORK/split "$ROOT/target/release/railwatch" split t
  TENANT=$WORK/split/telemetry/t
fi
"$DUMP" "$TENANT" "$WORK/cases.json" "$WORK/control.json" "$NOW" "$ENV_ID" "$WORK/rust.json" "${WARM:-0}"
python3 -I "$HERE/compare.py" "$WORK/rails.json" "$WORK/rust.json" ${VERBOSE:+--verbose} | tee "$WORK/diff.txt"
