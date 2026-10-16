#!/usr/bin/env bash
# Page timings, Rails vs Rust, on one large tenant file.
#
#   bench/diff/props/perf.sh TENANT.sqlite3 [warm_runs=3]
#
# TENANT is a multi-day file (build one with crates/load: rw-load
# --spread-days 9 against a Rust server; Phase 1 proved those rows identical
# to Rails'). This script:
# 1. creates a Rails scratch environment `perf-<run>`, stops writes to it,
#    and installs the file as its tenant (the gem's migrations are already
#    in it, so Rails opens it as-is);
# 2. runs settle_rails.rb (RollupJob/ReleaseHealthRollupJob for every hour,
#    exception grouping) so both sides read Rails' own rollups;
# 3. copies the settled file back out for Rust;
# 4. times every case WARM+1 times on both (first run dropped; Rails.cache
#    cleared before each run so grouped/summary_with_delta are computed, as
#    they are in Rust) and diffs the props too;
# 5. writes WORK/timings.md (median ms per case).
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../.." && pwd)
SRC=${1:?usage: perf.sh TENANT.sqlite3 [warm]}
WARM=${2:-3}
RUN=$(date +%s)
WORK=${WORK:-/tmp/rw-perf-$RUN}
mkdir -p "$WORK"
NOW=$(sqlite3 "$SRC" "SELECT CAST(strftime('%s', MAX(occurred_at)) AS INTEGER) + 60 FROM executions")
echo "$NOW" > "$WORK/now"

docker exec -i -e NAME="perf-$RUN" rw-baseline bin/rails runner - > "$WORK/env.txt" 2>/dev/null <<'RUBY'
account = Account.find_or_initialize_by(slug: "bench")
account.update!(name: "bench", plan: "business", monthly_event_quota: 1_000_000_000_000)
app = account.applications.find_or_create_by!(slug: "bench") { |a| a.name = "bench" }
env = app.environments.create!(name: ENV.fetch("NAME"))
env.update!(paused_at: Time.current) # no ingest into it while timing
puts "SLUG #{env.slug}"
puts "ENV_ID #{env.id}"
RUBY
SLUG=$(sed -n 's/^SLUG //p' "$WORK/env.txt"); ENV_ID=$(sed -n 's/^ENV_ID //p' "$WORK/env.txt")
echo "$SLUG" > "$WORK/slug"; echo "$ENV_ID" > "$WORK/env_id"
echo "rails env $SLUG (id $ENV_ID), now=$NOW"
DEST=/rails/storage/telemetry/$SLUG.sqlite3
docker exec rw-baseline sh -c "rm -f $DEST $DEST-wal $DEST-shm"
docker cp "$SRC" "rw-baseline:$DEST"
docker exec -u 0 rw-baseline chown rails:rails "$DEST"
# Pause stops ingest, not the jobs: settle the hours ourselves.
docker cp "$HERE/settle_rails.rb" rw-baseline:/tmp/settle_rails.rb
time docker exec -e SLUG="$SLUG" rw-baseline bin/rails runner /tmp/settle_rails.rb 2>/dev/null | grep SETTLED
docker exec -i -e SLUG="$SLUG" rw-baseline bin/rails runner - >/dev/null 2>&1 <<'RUBY'
Environment.find_by!(slug: ENV["SLUG"]).with_telemetry { Railwatch::TelemetryRecord.connection.execute("PRAGMA wal_checkpoint(TRUNCATE)") }
RUBY
docker cp "rw-baseline:$DEST" "$WORK/tenant.sqlite3"

echo '[]' > "$WORK/cases.json"
docker cp "$WORK/cases.json" rw-baseline:/tmp/props-cases.json
docker cp "$HERE/capture_rails.rb" rw-baseline:/tmp/capture_rails.rb
docker exec -e SLUG="$SLUG" -e NOW="$NOW" -e WARM=0 rw-baseline bin/rails runner /tmp/capture_rails.rb >/dev/null 2>&1
docker cp rw-baseline:/tmp/props-control.json "$WORK/control.json"
python3 -I "$HERE/make_cases.py" "$WORK/tenant.sqlite3" "$WORK/control.json" "$WORK/cases.json"
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
time docker exec -e SLUG="$SLUG" -e NOW="$NOW" -e WARM="$WARM" rw-baseline bin/rails runner /tmp/capture_rails.rb 2>/dev/null | grep CAPTURED
docker cp rw-baseline:/tmp/props-rails.json "$WORK/rails.json"
docker cp rw-baseline:/tmp/props-rails-timings.json "$WORK/rails.timings.json"

cargo build -q --release -p railwatch-server --bin props-dump --manifest-path "$ROOT/Cargo.toml"
"$ROOT/target/release/props-dump" "$WORK/tenant.sqlite3" "$WORK/cases.json" "$WORK/control.json" "$NOW" "$ENV_ID" "$WORK/rust.json" "$WARM"
python3 -I "$HERE/compare.py" "$WORK/rails.json" "$WORK/rust.json" > "$WORK/diff.txt" || true
tail -1 "$WORK/diff.txt"
python3 -I "$HERE/timings.py" "$WORK/rails.timings.json" "$WORK/rust.json.timings.json" | tee "$WORK/timings.md"
